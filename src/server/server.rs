use super::connection::Connection;
use crate::worker_threads::ReplayThreadPool;
use crate::{
    accept::producer::tcp_listen, config::Settings, replay::save::InnerReplaySaver,
    worker_threads::ReplayThreadContext,
};
use crate::{accept::ConnectionAcceptor, database::database::Database, replay::save::ReplaySaver};
use futures::{stream::StreamExt, Stream};
use log::debug;
use tokio::select;
use tokio_util::sync::CancellationToken;

fn real_server_deps(config: Settings) -> (impl Stream<Item = Connection>, Database) {
    let connections = tcp_listen(format!("localhost:{}", config.server.port));
    let database = Database::new(&config.database);
    (connections, database)
}

fn server_thread_pool(
    config: Settings,
    shutdown_token: CancellationToken,
    saver: ReplaySaver,
) -> ReplayThreadPool {
    let thread_count = config.server.worker_threads;
    let context = ReplayThreadContext::new(config, shutdown_token, saver);
    ReplayThreadPool::from_context(context, thread_count)
}

pub async fn run_server_with_deps(
    config: Settings,
    shutdown_token: CancellationToken,
    connections: impl Stream<Item = Connection>,
    database: Database,
) {
    let saver = InnerReplaySaver::new(database, config.clone());
    let thread_pool = server_thread_pool(config.clone(), shutdown_token.clone(), saver);
    let acceptor = ConnectionAcceptor::build(thread_pool, config);

    let accept_connections = connections.for_each_concurrent(None, |c| async {
        acceptor.accept(c).await;
    });
    select! {
        _ = accept_connections => { debug!("Server stopped accepting connections for some reason!") }
        _ = shutdown_token.cancelled() => { debug!("Server shutting down") }
    };
}

pub async fn run_server(config: Settings, shutdown_token: CancellationToken) {
    let (producer, database) = real_server_deps(config.clone());
    run_server_with_deps(config, shutdown_token, producer, database).await
}

#[cfg(test)]
mod test {
    use super::*;

    //    fn mock_conns() -> impl Stream<Item = Connection> {
    //        todo!()
    //    }

    fn tmp_dir() -> String {
        todo!()
    }
}
