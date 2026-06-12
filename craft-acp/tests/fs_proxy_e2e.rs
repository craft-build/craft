//! End-to-end test for `AcpFs`: spin up an in-memory ACP client/agent pair,
//! invoke `AcpFs::read_text_file` / `write_text_file` from the agent side,
//! and assert the client side received the corresponding `fs/*` requests.

use agent_client_protocol::role::acp::{Agent, Client};
use agent_client_protocol::schema::{
    ReadTextFileRequest, ReadTextFileResponse, SessionId, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{ByteStreams, ConnectionTo};
use craft_acp::fs_proxy::AcpFs;
use craft_agent::tools::FsBackend;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const SESSION: &str = "test-session";
const FILE_PATH: &str = "/virtual/example.txt";
const READ_CONTENT: &str = "hello from client";
const WRITE_CONTENT: &str = "hello from agent";

#[derive(Default)]
struct Captured {
    reads: Vec<(String, String)>,
    writes: Vec<(String, String, String)>,
}

#[tokio::test(flavor = "current_thread")]
async fn acp_fs_routes_read_and_write_to_client() {
    use tokio::task::LocalSet;

    let captured: Arc<Mutex<Captured>> = Arc::new(Mutex::new(Captured::default()));
    let captured_for_client = captured.clone();

    LocalSet::new()
        .run_until(async move {
            let (a_writer, c_reader) = tokio::io::duplex(64 * 1024);
            let (c_writer, a_reader) = tokio::io::duplex(64 * 1024);

            let agent_transport =
                ByteStreams::new(a_writer.compat_write(), a_reader.compat());
            let client_transport =
                ByteStreams::new(c_writer.compat_write(), c_reader.compat());

            let captured_read = captured_for_client.clone();
            let captured_write = captured_for_client.clone();

            tokio::task::spawn_local(async move {
                let _ = Client
                    .builder()
                    .on_receive_request(
                        async move |req: ReadTextFileRequest,
                                    responder,
                                    _cx: ConnectionTo<Agent>| {
                            captured_read.lock().unwrap().reads.push((
                                req.session_id.0.to_string(),
                                req.path.to_string_lossy().into_owned(),
                            ));
                            responder.respond(ReadTextFileResponse::new(READ_CONTENT))
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |req: WriteTextFileRequest,
                                    responder,
                                    _cx: ConnectionTo<Agent>| {
                            captured_write.lock().unwrap().writes.push((
                                req.session_id.0.to_string(),
                                req.path.to_string_lossy().into_owned(),
                                req.content,
                            ));
                            responder.respond(WriteTextFileResponse::new())
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .connect_to(client_transport)
                    .await;
            });

            Agent
                .builder()
                .connect_with(
                    agent_transport,
                    async |cx: ConnectionTo<Client>| -> Result<(), agent_client_protocol::Error> {
                        let fs = AcpFs::new(cx, SessionId::new(SESSION));

                        let read_back = fs
                            .read_text_file(&PathBuf::from(FILE_PATH))
                            .await
                            .expect("read_text_file");
                        assert_eq!(read_back, READ_CONTENT);

                        fs.write_text_file(&PathBuf::from(FILE_PATH), WRITE_CONTENT)
                            .await
                            .expect("write_text_file");

                        Ok(())
                    },
                )
                .await
                .expect("agent connection");
        })
        .await;

    let captured = captured.lock().unwrap();
    assert_eq!(captured.reads.len(), 1, "expected exactly one read request");
    assert_eq!(captured.reads[0].0, SESSION);
    assert_eq!(captured.reads[0].1, FILE_PATH);

    assert_eq!(captured.writes.len(), 1, "expected exactly one write request");
    assert_eq!(captured.writes[0].0, SESSION);
    assert_eq!(captured.writes[0].1, FILE_PATH);
    assert_eq!(captured.writes[0].2, WRITE_CONTENT);
}
