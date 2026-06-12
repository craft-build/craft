//! `FsBackend` implementation that routes reads/writes through the ACP client
//! via `fs/read_text_file` and `fs/write_text_file`. Used when the client
//! advertises both `fs.read_text_file` and `fs.write_text_file` in
//! `ClientCapabilities`. Otherwise `LocalFs` is used.

use std::path::Path;
use std::path::PathBuf;

use agent_client_protocol::ConnectionTo;
use agent_client_protocol::role::acp::Client;
use agent_client_protocol::schema::ReadTextFileRequest;
use agent_client_protocol::schema::SessionId;
use agent_client_protocol::schema::WriteTextFileRequest;
use craft_agent::tools::FsBackend;
use craft_agent::tools::FsFuture;

const READ_FAILED: &str = "acp fs/read_text_file failed";
const WRITE_FAILED: &str = "acp fs/write_text_file failed";

pub struct AcpFs {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
}

impl AcpFs {
    pub fn new(cx: ConnectionTo<Client>, session_id: SessionId) -> Self {
        Self { cx, session_id }
    }
}

impl FsBackend for AcpFs {
    fn read_text_file<'a>(&'a self, path: &'a Path) -> FsFuture<'a, String> {
        let path = PathBuf::from(path);
        Box::pin(async move {
            let req = ReadTextFileRequest::new(self.session_id.clone(), path);
            let resp = self
                .cx
                .send_request(req)
                .block_task()
                .await
                .map_err(|e| format!("{READ_FAILED}: {e}"))?;
            Ok(resp.content)
        })
    }

    fn write_text_file<'a>(&'a self, path: &'a Path, contents: &'a str) -> FsFuture<'a, ()> {
        let path = PathBuf::from(path);
        let contents = contents.to_owned();
        Box::pin(async move {
            let req = WriteTextFileRequest::new(self.session_id.clone(), path, contents);
            self.cx
                .send_request(req)
                .block_task()
                .await
                .map_err(|e| format!("{WRITE_FAILED}: {e}"))?;
            Ok(())
        })
    }
}
