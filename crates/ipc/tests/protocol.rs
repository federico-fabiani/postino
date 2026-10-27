//! End-to-end tests for the local transport: handshake, refusal, framing, cancellation, bounded
//! mutation slots and events. They use a real named pipe or Unix socket, never a TCP listener.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use postino_ipc::client::Client;
use postino_ipc::protocol::{HelloParams, RpcError, error_code, method};
use postino_ipc::server::{BoxFuture, DispatchRequest, Dispatcher, Server, ServerConfig};
use postino_ipc::{CancellationToken, DaemonPaths, EventHub, FrameReader, SharedToken, TokenFile, write_frame};
use serde_json::{Value, json};

struct TestDispatcher {
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl TestDispatcher {
    fn new() -> Self {
        Self { cancels: Mutex::new(HashMap::new()) }
    }
}

impl Dispatcher for TestDispatcher {
    fn dispatch(&self, request: DispatchRequest) -> BoxFuture<Result<Value, RpcError>> {
        if request.method == "op" {
            let name = request.params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            match name.as_str() {
                "slow" => {
                    let cancel = Arc::new(AtomicBool::new(false));
                    if let Ok(mut map) = self.cancels.lock() {
                        map.insert(request.request_id.clone(), Arc::clone(&cancel));
                    }
                    return Box::pin(async move {
                        for _ in 0..250 {
                            if cancel.load(Ordering::SeqCst) {
                                return Err(RpcError::cancelled());
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Ok(json!({ "finished": true }))
                    });
                }
                "mutate" => {
                    if !request.session.begin_mutation() {
                        return Box::pin(async { Err(RpcError::busy("one mutation at a time")) });
                    }
                    let session = Arc::clone(&request.session);
                    return Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        session.end_mutation();
                        Ok(json!({ "mutated": true }))
                    });
                }
                "echo" => return Box::pin(async move { Ok(request.params) }),
                "huge" => {
                    return Box::pin(async move { Ok(json!({ "data": "x".repeat(postino_ipc::MAX_FRAME_BYTES) })) });
                }
                other => {
                    let other = other.to_owned();
                    return Box::pin(async move { Err(RpcError::method_not_found(format!("unknown operation {other:?}"))) });
                }
            }
        }
        Box::pin(async move { Err(RpcError::method_not_found(format!("unknown method {:?}", request.method))) })
    }

    fn cancel(&self, request_id: &str) -> bool {
        self.cancels.lock().ok().and_then(|map| map.get(request_id).cloned()).is_some_and(|flag| {
            flag.store(true, Ordering::SeqCst);
            true
        })
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    paths: DaemonPaths,
    token: SharedToken,
    shutdown: CancellationToken,
    hub: Arc<EventHub>,
    task: tokio::task::JoinHandle<Result<(), postino_ipc::IpcError>>,
}

impl Fixture {
    async fn start() -> Self {
        Self::start_with(Duration::from_secs(120), 16).await
    }

    async fn start_with_idle(idle_timeout: Duration) -> Self {
        Self::start_with(idle_timeout, 16).await
    }

    async fn start_with(idle_timeout: Duration, global_in_flight: usize) -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = directory.path().join("mail.db");
        let paths = DaemonPaths::for_store(&store).expect("paths");
        let token = SharedToken::generate().expect("token");
        TokenFile::new(paths.token_file.clone()).write(&token).expect("token file");
        let hub = Arc::new(EventHub::new());
        let mut config = ServerConfig::new(paths.endpoint.clone(), token.clone());
        config.handshake_timeout = Duration::from_secs(2);
        config.idle_timeout = idle_timeout;
        config.global_in_flight = global_in_flight;
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(Server::new(config, Arc::new(TestDispatcher::new()), Arc::clone(&hub)).run(shutdown.clone()));
        // The pipe/socket is created by the server task; a connect retry covers the bind.
        for _ in 0..100 {
            if Client::connect(&paths, "test", Duration::from_millis(20), Duration::from_secs(2)).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self { _directory: directory, paths, token, shutdown, hub, task }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.paths, "test", Duration::from_secs(2), Duration::from_secs(2)).await.expect("client")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

#[tokio::test]
async fn a_correct_token_connects_and_ping_answers() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    assert_eq!(client.hello().protocol, postino_ipc::PROTOCOL_VERSION);
    let pong = client.ping(Duration::from_secs(2)).await.expect("ping");
    assert_eq!(pong["ok"], true);
}

#[tokio::test]
async fn a_wrong_token_is_a_generic_refusal() {
    let fixture = Fixture::start().await;
    TokenFile::new(fixture.paths.token_file.clone()).write(&SharedToken::generate().expect("token")).expect("wrong token");
    let result = Client::connect(&fixture.paths, "test", Duration::from_secs(2), Duration::from_secs(2)).await;
    assert!(matches!(result, Err(postino_ipc::IpcError::Unauthorized)), "{result:?}");
    // Put the real token back so the fixture teardown and the next test are independent.
    TokenFile::new(fixture.paths.token_file.clone()).write(&fixture.token).expect("restore");
}

#[tokio::test]
async fn a_version_mismatch_is_readable_and_does_not_authenticate() {
    let fixture = Fixture::start().await;
    let stream = postino_ipc::client::connect_raw(&fixture.paths.endpoint, Duration::from_secs(2)).await.expect("stream");
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader, postino_ipc::MAX_FRAME_BYTES);
    let hello = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": method::HELLO,
        "params": HelloParams { protocol: 999, client: "test".to_owned(), token: plain_token(&fixture), allow_send: true }
    });
    write_frame(&mut writer, &hello, postino_ipc::MAX_FRAME_BYTES).await.expect("hello");
    let frame = reader.next_frame().await.expect("frame").expect("response");
    let value: Value = serde_json::from_slice(&frame).expect("json");
    assert_eq!(value["error"]["code"], error_code::VERSION);
    assert!(value["error"]["message"].as_str().is_some_and(|message| message.contains("999")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_frame_is_refused_without_hanging_the_connection() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    // A real client refuses to produce this frame; use the raw writer to prove the server does too.
    let _ = client.ping(Duration::from_secs(2)).await.expect("ping");
    let stream = postino_ipc::client::connect_raw(&fixture.paths.endpoint, Duration::from_secs(2)).await.expect("stream");
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = FrameReader::new(reader, postino_ipc::MAX_FRAME_BYTES);
    let hello = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method::HELLO,
        "params": HelloParams { protocol: postino_ipc::PROTOCOL_VERSION, client: "test".to_owned(), token: plain_token(&fixture), allow_send: true }
    });
    write_frame(&mut writer, &hello, postino_ipc::MAX_FRAME_BYTES).await.expect("hello");
    let _ = reader.next_frame().await.expect("hello response");
    // Prove the raw connection still carries a normal frame before the oversized one.
    write_frame(&mut writer, &json!({ "jsonrpc": "2.0", "id": 2, "method": "ping", "params": {} }), postino_ipc::MAX_FRAME_BYTES)
        .await
        .expect("ping");
    let pong = reader.next_frame().await.expect("pong").expect("pong response");
    assert!(String::from_utf8_lossy(&pong).contains("\"ok\":true"));
    let mut giant = vec![b'x'; postino_ipc::MAX_FRAME_BYTES + 1];
    giant.push(b'\n');
    use tokio::io::AsyncWriteExt as _;
    writer.write_all(&giant).await.expect("giant frame");
    writer.flush().await.expect("flush");
    let frame = reader.next_frame().await.expect("error frame").expect("response");
    let value: Value = serde_json::from_slice(&frame).expect("json");
    assert_eq!(value["error"]["code"], error_code::FRAME_TOO_LARGE);
}

#[tokio::test]
async fn one_mutation_per_client_is_enforced() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    let first = client.start_request(method::OP, json!({ "name": "mutate", "args": {} })).await.expect("first");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let second = client.start_request(method::OP, json!({ "name": "mutate", "args": {} })).await.expect("second");
    let second_value = client.wait_for(second, Duration::from_secs(2)).await;
    assert!(matches!(second_value, Err(postino_ipc::IpcError::Busy(_))), "{second_value:?}");
    let first_value = client.wait_for(first, Duration::from_secs(2)).await.expect("first completes");
    assert_eq!(first_value["mutated"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_response_is_a_readable_error_and_keeps_the_connection() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    let result = client.call(method::OP, json!({ "name": "huge", "args": {} }), Duration::from_secs(10)).await;
    match result {
        Err(postino_ipc::IpcError::Rpc(error)) => assert_eq!(error.code, error_code::FRAME_TOO_LARGE),
        other => panic!("expected a FRAME_TOO_LARGE error, got {other:?}"),
    }
    assert!(client.ping(Duration::from_secs(2)).await.is_ok(), "the oversized response closed the connection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thirty_two_real_clients_are_accepted_and_the_listener_survives() {
    let fixture = Fixture::start().await;
    let mut clients = Vec::new();
    for index in 0..32 {
        let client = Client::connect(&fixture.paths, "burst", Duration::from_secs(2), Duration::from_secs(5))
            .await
            .unwrap_or_else(|error| panic!("client {index}: {error}"));
        clients.push(client);
    }
    // The 33rd connection is the documented overload, not a listener exit.
    let over = Client::connect(&fixture.paths, "overflow", Duration::from_secs(2), Duration::from_secs(5)).await;
    assert!(matches!(over, Err(postino_ipc::IpcError::Overloaded)), "{over:?}");
    assert!(!fixture.task.is_finished(), "the listener died at the connection cap");
    drop(clients);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!fixture.task.is_finished(), "the listener died after the burst");
    let mut after = fixture.client().await;
    assert!(after.ping(Duration::from_secs(2)).await.is_ok(), "the listener did not recover after the burst");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thirty_one_unauthenticated_pipes_do_not_kill_the_listener() {
    let fixture = Fixture::start().await;
    let mut raw = Vec::new();
    for index in 0..31 {
        let stream = postino_ipc::client::connect_raw(&fixture.paths.endpoint, Duration::from_secs(2))
            .await
            .unwrap_or_else(|error| panic!("raw {index}: {error}"));
        raw.push(stream);
    }
    // The 32nd slot is a real handshake; it must still work.
    let mut real = fixture.client().await;
    assert!(real.ping(Duration::from_secs(2)).await.is_ok());
    drop(raw);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!fixture.task.is_finished(), "the listener died after unauthenticated connections");
    assert!(real.ping(Duration::from_secs(2)).await.is_ok(), "the authenticated connection was lost");
}

#[tokio::test]
async fn a_client_cannot_cancel_another_connections_request() {
    let fixture = Fixture::start().await;
    let mut first = fixture.client().await;
    let id = first.start_request(method::OP, json!({ "name": "slow", "args": {} })).await.expect("slow");
    let mut second = fixture.client().await;
    let response = second.call(method::CANCEL, json!({ "request": id.to_string() }), Duration::from_secs(2)).await.expect("cancel");
    assert_eq!(response["cancelled"], false);
    let result = first.wait_for(id, Duration::from_secs(10)).await.expect("the first request was not cancelled");
    assert_eq!(result["finished"], true);
}

#[tokio::test]
async fn a_duplicate_live_request_id_is_refused() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    let id = client.start_request_with_id(77, method::OP, json!({ "name": "slow", "args": {} })).await.expect("first");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let duplicate = client.start_request_with_id(77, method::OP, json!({ "name": "echo", "args": {} })).await.expect("send duplicate");
    let response = client.wait_for(duplicate, Duration::from_secs(2)).await;
    assert!(matches!(response, Err(postino_ipc::IpcError::Rpc(ref error)) if error.code == error_code::INVALID_REQUEST), "{response:?}");
    let _ = client.wait_for(id, Duration::from_secs(3)).await;
}

#[tokio::test]
async fn a_client_can_cancel_its_own_in_flight_request() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    let id = client.start_request(method::OP, json!({ "name": "slow", "args": {} })).await.expect("slow");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let response = client.call(method::CANCEL, json!({ "request": id.to_string() }), Duration::from_secs(2)).await.expect("cancel");
    assert_eq!(response["cancelled"], true);
    let result = client.wait_for(id, Duration::from_secs(2)).await;
    assert!(matches!(result, Err(postino_ipc::IpcError::Cancelled)), "{result:?}");
}

#[tokio::test]
async fn events_are_delivered_to_a_subscriber() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    let _subscription = client.subscribe(Duration::from_secs(2)).await.expect("subscribe");
    fixture.hub.publish("index", json!({ "indexed": 500, "total": 1200 }));
    let event = client.next_event(Duration::from_secs(2)).await.expect("event").expect("event");
    assert_eq!(event["kind"], "index");
    assert_eq!(event["data"]["indexed"], 500);
}

#[tokio::test]
async fn a_global_dispatch_cap_refuses_excess_reads() {
    let fixture = Fixture::start_with(Duration::from_secs(120), 1).await;
    let mut first = fixture.client().await;
    let id = first.start_request(method::OP, json!({ "name": "slow", "args": {} })).await.expect("slow");
    tokio::time::sleep(Duration::from_millis(80)).await;
    let mut second = fixture.client().await;
    let result = second.call(method::OP, json!({ "name": "slow", "args": {} }), Duration::from_secs(2)).await;
    assert!(matches!(result, Err(postino_ipc::IpcError::Overloaded)), "{result:?}");
    let _ = first.wait_for(id, Duration::from_secs(10)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_operation_is_not_killed_by_the_idle_timeout() {
    let fixture = Fixture::start_with(Duration::from_millis(300), 16).await;
    let mut client = fixture.client().await;
    let id = client.start_request(method::OP, json!({ "name": "slow", "args": {} })).await.expect("slow");
    let result = client.wait_for(id, Duration::from_secs(10)).await.expect("a 2.5 s dispatch outlives a 300 ms idle timeout");
    assert_eq!(result["finished"], true);
    assert!(client.ping(Duration::from_secs(2)).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_subscriber_is_kept_alive_by_the_heartbeat() {
    let fixture = Fixture::start_with(Duration::from_millis(300), 16).await;
    let mut client = fixture.client().await;
    client.subscribe(Duration::from_secs(2)).await.expect("subscribe");
    tokio::time::sleep(Duration::from_millis(700)).await;
    // No event is due; `next_event` returns None after its wait, and the connection must still work.
    let event = client.next_event(Duration::from_millis(100)).await.expect("subscription read");
    assert!(event.is_none());
    assert!(client.ping(Duration::from_secs(2)).await.is_ok(), "the idle subscription was closed");
}

#[tokio::test]
async fn subscriptions_are_capped_per_connection() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    for index in 0..4 {
        client.subscribe(Duration::from_secs(2)).await.unwrap_or_else(|error| panic!("subscription {index}: {error}"));
    }
    let over = client.subscribe(Duration::from_secs(2)).await;
    assert!(matches!(over, Err(postino_ipc::IpcError::Overloaded)), "{over:?}");
    assert!(client.ping(Duration::from_secs(2)).await.is_ok());
}

#[tokio::test]
async fn ping_keeps_an_idle_connection_alive_and_silence_is_closed() {
    let fixture = Fixture::start_with_idle(Duration::from_millis(400)).await;
    let mut alive = fixture.client().await;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(alive.ping(Duration::from_secs(1)).await.is_ok(), "a heartbeat must keep the connection alive");
    }
    let mut quiet = fixture.client().await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let result = quiet.ping(Duration::from_secs(1)).await;
    assert!(matches!(result, Err(postino_ipc::IpcError::Closed) | Err(postino_ipc::IpcError::Io(_))), "{result:?}");
}

#[tokio::test]
async fn event_subscription_kinds_filter_the_stream() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    client.call(method::EVENTS_SUBSCRIBE, json!({ "kinds": ["index"] }), Duration::from_secs(2)).await.expect("subscribe");
    fixture.hub.publish("sync", json!({ "phase": "started" }));
    fixture.hub.publish("index", json!({ "indexed": 10, "total": 20 }));
    let event = client.next_event(Duration::from_secs(2)).await.expect("event").expect("index event");
    assert_eq!(event["kind"], "index");
}

#[tokio::test]
async fn unknown_operations_are_readable_errors() {
    let fixture = Fixture::start().await;
    let mut client = fixture.client().await;
    let result = client.call(method::OP, json!({ "name": "not_an_operation", "args": {} }), Duration::from_secs(2)).await;
    match result {
        Err(postino_ipc::IpcError::Rpc(error)) => {
            assert_eq!(error.code, error_code::METHOD_NOT_FOUND);
            assert!(error.message.contains("not_an_operation"));
        }
        other => panic!("unexpected result {other:?}"),
    }
}

fn plain_token(fixture: &Fixture) -> String {
    // The test file is the only place that needs the plaintext, and it is never logged.
    std::fs::read_to_string(&fixture.paths.token_file).expect("token file").trim().to_owned()
}
