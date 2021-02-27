use std::cell::Cell;

use tokio::join;
use tokio_util::sync::CancellationToken;
use tokio::time::Duration;

use crate::{
    accept::header::ConnectionType,
    config::Settings,
    server::connection::Connection,
    util::{empty_counter::EmptyCounter, timeout::cancellable},
};

use super::{receive::ReplayMerger, save::ReplaySaver, send::ReplaySender};

pub struct Replay {
    id: u64,
    merger: ReplayMerger,
    sender: ReplaySender,
    saver: ReplaySaver,
    replay_timeout_token: CancellationToken,
    writer_connection_count: EmptyCounter,
    reader_connection_count: EmptyCounter,
    time_with_zero_writers_to_end_replay: Duration,
    forced_timeout: Duration,
    should_stop_accepting_connections: Cell<bool>,
}

impl Replay {
    pub fn new(
        id: u64,
        shutdown_token: CancellationToken,
        config: Settings,
        saver: ReplaySaver,
    ) -> Self {
        let writer_connection_count = EmptyCounter::new();
        let reader_connection_count = EmptyCounter::new();
        let should_stop_accepting_connections = Cell::new(false);
        let time_with_zero_writers_to_end_replay =
            Duration::from_secs(config.replay.time_with_zero_writers_to_end_replay_s);
        let forced_timeout = Duration::from_secs(config.replay.forced_timeout_s);
        let replay_timeout_token = shutdown_token.child_token();

        let merger = ReplayMerger::new(replay_timeout_token.clone(), config);
        let merged_replay = merger.get_merged_replay();
        let sender = ReplaySender::new(merged_replay, replay_timeout_token.clone());

        Self {
            id,
            merger,
            sender,
            saver,
            replay_timeout_token,
            writer_connection_count,
            reader_connection_count,
            time_with_zero_writers_to_end_replay,
            forced_timeout,
            should_stop_accepting_connections,
        }
    }

    async fn timeout(&self) {
        let cancellation = async {
            tokio::time::sleep(self.forced_timeout).await;
            self.replay_timeout_token.cancel();
            log::debug!("Replay {} timed out", self.id);
        };

        // Return early if we got cancelled normally
        cancellable(cancellation, &self.replay_timeout_token).await;
    }

    async fn wait_until_there_were_no_writers_for_a_while(&self) {
        let wait = self
            .writer_connection_count
            .wait_until_empty_for(self.time_with_zero_writers_to_end_replay);
        // We don't have to return when there are no writers, just when we shouldn't accept more.
        cancellable(wait, &self.replay_timeout_token).await;
    }

    async fn regular_lifetime(&self) {
        log::debug!("Replay {} started", self.id);
        self.wait_until_there_were_no_writers_for_a_while().await;
        self.should_stop_accepting_connections.set(true);
        log::debug!("Replay {} stopped accepting connections", self.id);
        self.writer_connection_count.wait_until_empty().await;
        self.merger.finalize();
        log::debug!("Replay {} finished merging the replay", self.id);
        self.saver
            .save_replay(self.merger.get_merged_replay(), self.id)
            .await;
        log::debug!("Replay {} saved the replay", self.id);
        self.reader_connection_count.wait_until_empty().await;
        log::debug!("Replay {} is finished", self.id);
        // Cancel to return from timeout
        self.replay_timeout_token.cancel();
    }

    pub async fn lifetime(&self) {
        join! {
            self.regular_lifetime(),
            self.timeout(),
        };
    }

    pub async fn handle_connection(&self, mut c: Connection) -> () {
        if self.should_stop_accepting_connections.get() {
            log::info!(
                "Replay {} dropped a connection because its write phase is over",
                self.id
            );
            return;
        }
        match c.get_header().type_ {
            ConnectionType::WRITER => {
                self.writer_connection_count.inc();
                self.merger.handle_connection(&mut c).await;
                self.writer_connection_count.dec();
            }
            ConnectionType::READER => {
                self.reader_connection_count.inc();
                self.sender.handle_connection(&mut c).await;
                self.reader_connection_count.dec();
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::{
        accept::header::ConnectionHeader,
        config::test::default_config,
        replay::save::InnerReplaySaver,
        server::connection::test::test_connection,
        util::test::{get_file, setup_logging},
    };

    #[tokio::test]
    async fn test_replay_forced_timeout() {
        setup_logging();
        tokio::time::pause();

        let mut mock_saver = InnerReplaySaver::faux();
        faux::when!(mock_saver.save_replay).then_do(|| ());

        let token = CancellationToken::new();
        let mut config = default_config();
        config.replay.forced_timeout_s = 3600;

        let (mut c, _r, _w) = test_connection();
        let c_header = ConnectionHeader {
            type_: ConnectionType::WRITER,
            id: 1,
            name: "foo".into(),
        };
        c.set_header(c_header);

        let replay = Replay::new(1, token, Arc::new(config), Arc::new(mock_saver));

        let replay_ended = Cell::new(false);
        let run_replay = async {
            join! {
                replay.lifetime(),
                replay.handle_connection(c),
            };
            replay_ended.set(true);
        };
        let check_result = async {
            tokio::time::sleep(Duration::from_secs(3599)).await;
            assert!(!replay_ended.get());
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert!(replay_ended.get());
        };

        join! { run_replay, check_result };
    }

    #[tokio::test]
    async fn test_replay_one_writer_one_reader() {
        setup_logging();
        tokio::time::pause();

        let mut mock_saver = InnerReplaySaver::faux();
        faux::when!(mock_saver.save_replay).then_do(|| ());
        let token = CancellationToken::new();
        let config = default_config();

        let (mut c_read, mut reader, mut _w) = test_connection();
        let (mut c_write, _r, mut writer) = test_connection();
        c_write.set_header(ConnectionHeader {
            type_: ConnectionType::WRITER,
            id: 1,
            name: "foo".into(),
        });
        c_read.set_header(ConnectionHeader {
            type_: ConnectionType::READER,
            id: 1,
            name: "foo".into(),
        });

        let replay = Replay::new(1, token, Arc::new(config), Arc::new(mock_saver));
        let run_replay = async {
            join! {
                replay.lifetime(),
                replay.handle_connection(c_write),
                async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    replay.handle_connection(c_read).await;
                }
            };
        };

        let example_replay_file = get_file("example");
        let replay_writing = async {
            for data in example_replay_file.chunks(100) {
                writer.write_all(data).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            drop(writer);
        };
        let mut received_replay_file = Vec::<u8>::new();
        let replay_reading = async {
            reader.read_to_end(&mut received_replay_file).await.unwrap();
        };

        join! { run_replay, replay_reading, replay_writing };

        // TODO make a utility method
        if example_replay_file.len() != received_replay_file.len() {
            panic!(
                "Length mismatch: {} != {}",
                example_replay_file.len(),
                received_replay_file.len()
            )
        }
        for (i, (c1, c2)) in example_replay_file
            .iter()
            .zip(received_replay_file.iter())
            .enumerate()
        {
            if c1 != c2 {
                panic!("Buffers differ at byte {}: {} != {}", i, c1, c2);
            }
        }
    }
}
