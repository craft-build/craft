use craft_agent::EmbedRequest;
use std::sync::Arc;

pub type EmbedChannel = Arc<flume::Sender<EmbedRequest>>;

#[cfg(feature = "onnx")]
pub(crate) fn create_embed_table(
    lua: &mlua::Lua,
    embed_tx: EmbedChannel,
) -> mlua::Result<mlua::Table> {
    use mlua::{Table, Value};

    let table = lua.create_table()?;

    let embed_tx_clone = embed_tx.clone();
    table.set(
        "embed",
        lua.create_async_function(move |lua, text: String| {
            let tx = embed_tx_clone.clone();
            async move {
                let (reply_tx, reply_rx) =
                    tokio::sync::oneshot::channel::<Result<Vec<f32>, String>>();
                if tx.send((text, reply_tx)).is_err() {
                    return Err(mlua::Error::external("embed service unavailable"));
                }
                match reply_rx.await {
                    Ok(Ok(vec)) => {
                        let table = lua.create_table_with_capacity(vec.len(), 0)?;
                        for (i, val) in vec.iter().enumerate() {
                            table.set(i + 1, *val)?;
                        }
                        Ok(Value::Table(table))
                    }
                    Ok(Err(e)) => Err(mlua::Error::external(e)),
                    Err(_) => Err(mlua::Error::external("embed service dropped")),
                }
            }
        })?,
    )?;

    table.set(
        "similarity",
        lua.create_function(|_, (a, b): (Table, Table)| {
            let vec_a: Vec<f32> = a.sequence_values::<f32>().filter_map(|v| v.ok()).collect();
            let vec_b: Vec<f32> = b.sequence_values::<f32>().filter_map(|v| v.ok()).collect();
            Ok(craft_agent::agent::cosine_similarity(&vec_a, &vec_b))
        })?,
    )?;

    Ok(table)
}
