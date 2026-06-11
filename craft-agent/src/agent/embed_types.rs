use tokio::sync::oneshot;

pub type EmbedRequest = (String, oneshot::Sender<Result<Vec<f32>, String>>);
