//! No tokio runtime anywhere, as in a `GLib` main-loop client; a thread-parking executor stands
//! in for `glib::MainContext`. Run with `cargo test -p apex-dbus --features async-io`.

#![allow(
    clippy::expect_used,
    reason = "integration test helpers: a failed expectation is the test failure"
)]
#![cfg(feature = "async-io")]

mod common;

use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread::{self, Thread},
};

use apex_dbus::{ApexError, OBJECT_PATH, State, Update1Proxy};
use common::{Log, MockDaemon, STAGED_DIGEST};
use zbus::{Guid, connection, proxy::CacheProperties};

struct Unparker(Thread);

impl Wake for Unparker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unparker(thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        thread::park();
    }
}

async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut a_out, mut b_out) = (None, None);
    poll_fn(|context| {
        if a_out.is_none()
            && let Poll::Ready(output) = a.as_mut().poll(context)
        {
            a_out = Some(output);
        }
        if b_out.is_none()
            && let Poll::Ready(output) = b.as_mut().poll(context)
        {
            b_out = Some(output);
        }
        match (a_out.take(), b_out.take()) {
            (Some(a), Some(b)) => Poll::Ready((a, b)),
            (a, b) => {
                (a_out, b_out) = (a, b);
                Poll::Pending
            }
        }
    })
    .await
}

#[test]
fn proxy_runs_without_a_tokio_runtime() {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "this test must run outside any tokio runtime"
    );
    let log = Log::default();
    let (server_socket, client_socket) =
        std::os::unix::net::UnixStream::pair().expect("socket pair");

    block_on(async {
        let server = connection::Builder::unix_stream(server_socket)
            .server(Guid::generate())
            .p2p()
            .serve_at(OBJECT_PATH, MockDaemon::new(log.clone()))
            .build();
        let client = connection::Builder::unix_stream(client_socket)
            .p2p()
            .build();
        let (server, client) = join(server, client).await;
        let (_server, client) = (server.expect("server"), client.expect("client"));

        let proxy = Update1Proxy::builder(&client)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("proxy");
        proxy.ping().await.expect("Ping");
        assert_eq!(proxy.state().await.expect("State"), State::Staged);
        assert_eq!(
            proxy.get_changes().await.expect("GetChanges"),
            common::changes()
        );
        proxy.apply(STAGED_DIGEST).await.expect("Apply");
        assert!(matches!(
            proxy.apply("sha256:0000").await,
            Err(ApexError::DigestMismatch(_))
        ));
    });

    let members: Vec<String> = log.calls().into_iter().map(|call| call.member).collect();
    assert_eq!(members, ["Ping", "GetChanges", "Apply", "Apply"]);
}
