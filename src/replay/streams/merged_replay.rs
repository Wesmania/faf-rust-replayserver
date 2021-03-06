use std::{cell::RefCell, io::Read, io::Write, rc::Rc};

use futures::Future;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::{
    util::buf_traits::DiscontiguousBuf,
    util::{
        buf_deque::BufDeque,
        buf_traits::{DiscontiguousBufExt, ReadAt, ReadAtExt},
        event::Event,
    },
};

use super::{writer_replay::WriterReplay, ReplayHeader};

pub struct MergedReplay {
    data: BufDeque,
    header: Option<ReplayHeader>,
    delayed_data_len: usize,
    finished: bool,
    delayed_data_notification: Event,
}

impl MergedReplay {
    pub fn new() -> Self {
        Self {
            data: BufDeque::new(),
            header: None,
            delayed_data_len: 0,
            finished: false,
            delayed_data_notification: Event::new(),
        }
    }

    pub fn header_len(&self) -> usize {
        self.get_header().map_or(0, |h| h.data.len())
    }

    pub fn delayed_data_len(&self) -> usize {
        self.delayed_data_len
    }

    pub fn delayed_len(&self) -> usize {
        self.delayed_data_len + self.header_len()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn wait_for_more_data(&self) -> impl Future<Output = ()> {
        let wait = if self.finished {
            None
        } else {
            Some(self.delayed_data_notification.wait())
        };
        async move {
            if let Some(w) = wait {
                w.await;
            }
        }
    }

    pub fn add_header(&mut self, header: ReplayHeader) {
        debug_assert!(!self.finished);
        debug_assert!(self.get_data().len() == 0);
        self.header = Some(header);
        self.delayed_data_notification.notify();
    }

    pub fn get_header(&self) -> Option<&ReplayHeader> {
        self.header.as_ref()
    }

    pub fn add_data(&mut self, writer: &WriterReplay, until: usize) {
        debug_assert!(!self.finished);
        debug_assert!(until <= writer.get_data().len());

        let writer_data = writer.get_data();
        let from = self.data.len();
        for chunk in writer_data.iter_chunks(from, until) {
            self.data.write_all(chunk).unwrap();
        }
    }

    pub fn get_data(&self) -> &impl DiscontiguousBuf {
        &self.data
    }

    pub fn advance_delayed_data(&mut self, len: usize) {
        debug_assert!(len <= self.data.len());
        debug_assert!(!self.finished);
        self.delayed_data_len = len;
        self.delayed_data_notification.notify();
    }

    pub fn finish(&mut self) {
        self.finished = true;
        self.delayed_data_notification.notify();
    }
}

impl ReadAt for MergedReplay {
    fn read_at(&self, mut start: usize, buf: &mut [u8]) -> std::io::Result<usize> {
        if start >= self.delayed_len() {
            return Ok(0);
        }
        debug_assert!(self.header.is_some());
        if start < self.header_len() {
            let mut data = &self.get_header().unwrap().data[start..];
            data.read(buf)
        } else {
            start -= self.header_len();
            let read_max = std::cmp::min(buf.len(), self.delayed_data_len - start);
            self.data.read_at(start, &mut buf[..read_max])
        }
    }
}

pub type MReplayRef = Rc<RefCell<MergedReplay>>;

pub async fn write_replay_stream(replay: &MReplayRef, c: &mut (impl AsyncWrite + Unpin)) -> std::io::Result<()> {
    let mut buf: Box<[u8]> = Box::new([0; 4096]);
    let mut reader = replay.reader();
    loop {
        let r = replay.borrow();
        if r.delayed_len() <= reader.position() && r.is_finished() {
            return Ok(());
        }
        drop(r);

        let data_read = reader.read(&mut *buf).unwrap();
        c.write_all(&buf[..data_read]).await?;
        if data_read == 0 {
            let f = replay.borrow().wait_for_more_data();
            f.await;
        }
    }
}

// TODO tests
