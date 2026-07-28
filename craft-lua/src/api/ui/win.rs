use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mlua::{AnyUserData, Lua, Result as LuaResult, Table, UserData, UserDataMethods};

use super::{parse_footer, try_parse_dimension};
use crate::api::util::command::{Anchor, Border, FloatConfigPatch, TitlePos, WinCommand, WinEvent};

/// All mutable state is in `Cell`s so every Lua method takes a shared
/// borrow and `recv` never needs to re-borrow mutably after waking.
/// mlua's userdata lock is exclusive even for shared borrows, so `recv`
/// additionally must not hold any borrow across its await; see below.
pub(crate) struct WinHandle {
    event_rx: flume::Receiver<WinEvent>,
    cmd_tx: flume::Sender<WinCommand>,
    closed: AtomicBool,
    visible: AtomicBool,
    init_width: u16,
    init_height: u16,
}

impl WinHandle {
    pub fn new(
        event_rx: flume::Receiver<WinEvent>,
        cmd_tx: flume::Sender<WinCommand>,
        init_width: u16,
        init_height: u16,
        visible: bool,
    ) -> Self {
        Self {
            event_rx,
            cmd_tx,
            closed: AtomicBool::new(false),
            visible: AtomicBool::new(visible),
            init_width,
            init_height,
        }
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(flume::TrySendError::Full(cmd)) = self.cmd_tx.try_send(WinCommand::Close) {
            let _ = self.cmd_tx.send(cmd);
        }
    }

    fn send(&self, cmd: WinCommand) {
        if let Err(flume::TrySendError::Disconnected(_)) = self.cmd_tx.try_send(cmd) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        self.close();
    }
}

fn tagged(lua: &Lua, ty: &str) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    tbl.set("type", ty)?;
    Ok(tbl)
}

fn event_table(lua: &Lua, event: WinEvent) -> LuaResult<Table> {
    match event {
        WinEvent::Key { key } => {
            let tbl = tagged(lua, "key")?;
            tbl.set("key", key)?;
            Ok(tbl)
        }
        WinEvent::Resize { width, height } => {
            let tbl = tagged(lua, "resize")?;
            tbl.set("width", width)?;
            tbl.set("height", height)?;
            Ok(tbl)
        }
        WinEvent::Paste { text } => {
            let tbl = tagged(lua, "paste")?;
            tbl.set("text", text)?;
            Ok(tbl)
        }
        WinEvent::Close => tagged(lua, "close"),
    }
}

impl UserData for WinHandle {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("width", |_, this| Ok(this.init_width));
        fields.add_field_method_get("height", |_, this| Ok(this.init_height));
        fields.add_field_method_get("visible", |_, this| Ok(this.visible.load(Ordering::SeqCst)));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // recv() blocks until the next event; recv(timeout_ms) additionally
        // resolves to `{ type = "timeout" }` so plugins can animate.
        //
        // Registered as an async function (not method) on purpose: an async
        // method's userdata borrow is held across the await, and mlua's lock
        // rejects ALL other borrows meanwhile (shared ones included), so any
        // win call from another coroutine would fail while a recv is parked,
        // which is virtually always for an event-loop plugin. Only the
        // cloned receiver is kept across the suspension.
        methods.add_async_function(
            "recv",
            |lua, (ud, timeout_ms): (AnyUserData, Option<u64>)| async move {
                let rx = {
                    let this = ud.borrow::<WinHandle>()?;
                    if this.closed.load(Ordering::SeqCst) {
                        return Ok(mlua::Value::Nil);
                    }
                    this.event_rx.clone()
                };
                let event = match timeout_ms {
                    Some(ms) => {
                        let sleep = tokio::time::sleep(Duration::from_millis(ms));
                        tokio::select! {
                            biased;
                            res = rx.recv_async() => Some(res),
                            () = sleep => None,
                        }
                    }
                    None => Some(rx.recv_async().await),
                };
                match event {
                    Some(Ok(event)) => {
                        if matches!(event, WinEvent::Close) {
                            ud.borrow::<WinHandle>()?
                                .closed
                                .store(true, Ordering::SeqCst);
                        }
                        Ok(mlua::Value::Table(event_table(&lua, event)?))
                    }
                    Some(Err(_)) => {
                        ud.borrow::<WinHandle>()?
                            .closed
                            .store(true, Ordering::SeqCst);
                        Ok(mlua::Value::Nil)
                    }
                    None => Ok(mlua::Value::Table(tagged(&lua, "timeout")?)),
                }
            },
        );

        methods.add_method("set_config", |_, this, opts: Table| {
            if this.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            let mut patch = FloatConfigPatch::default();
            if let Ok(t) = opts.get::<String>("title") {
                patch.title = Some(t);
            }
            if let Ok(f) = parse_footer(&opts)
                && !f.is_empty()
            {
                patch.footer = Some(f);
            }
            if let Ok(b) = opts.get::<String>("border") {
                patch.border = Some(Border::parse(&b));
            }
            if let Ok(tp) = opts.get::<String>("title_pos") {
                patch.title_pos = Some(TitlePos::parse(&tp));
            }
            if let Ok(a) = opts.get::<String>("anchor") {
                patch.anchor = Some(Anchor::parse(&a));
            }
            if let Ok(z) = opts.get::<u16>("zindex") {
                patch.zindex = Some(z);
            }
            if let Ok(o) = opts.get::<u16>("order") {
                patch.order = Some(o);
            }
            patch.width = try_parse_dimension(&opts, "width");
            patch.height = try_parse_dimension(&opts, "height");
            this.send(WinCommand::SetConfig(patch));
            Ok(())
        });

        methods.add_method("set_cursor", |_, this, row: usize| {
            if this.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            this.send(WinCommand::SetCursor(row.saturating_sub(1)));
            Ok(())
        });

        methods.add_method("close", |_, this, ()| {
            this.close();
            Ok(())
        });

        methods.add_method("is_open", |_, this, ()| {
            if !this.closed.load(Ordering::SeqCst) && this.cmd_tx.is_disconnected() {
                this.closed.store(true, Ordering::SeqCst);
            }
            Ok(!this.closed.load(Ordering::SeqCst))
        });

        methods.add_method("show", |_, this, ()| {
            if this.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            this.visible.store(true, Ordering::SeqCst);
            this.send(WinCommand::SetVisible(true));
            Ok(())
        });

        methods.add_method("hide", |_, this, ()| {
            if this.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            this.visible.store(false, Ordering::SeqCst);
            this.send(WinCommand::SetVisible(false));
            Ok(())
        });

        methods.add_method("is_visible", |_, this, ()| {
            if !this.closed.load(Ordering::SeqCst) && this.cmd_tx.is_disconnected() {
                this.closed.store(true, Ordering::SeqCst);
            }
            Ok(this.visible.load(Ordering::SeqCst) && !this.closed.load(Ordering::SeqCst))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channels() -> (
        flume::Sender<WinEvent>,
        flume::Receiver<WinCommand>,
        WinHandle,
    ) {
        let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
        let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);
        let handle = WinHandle::new(event_rx, cmd_tx, 80, 24, true);
        (event_tx, cmd_rx, handle)
    }

    #[test]
    fn close_is_idempotent_including_drop() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        handle.close();
        assert!(handle.closed.load(Ordering::SeqCst));
        handle.close();
        drop(handle);
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::Close)));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn drop_auto_closes() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        drop(handle);
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::Close)));
    }

    #[test]
    fn drop_after_close_does_not_resend() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        handle.close();
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::Close)));
        drop(handle);
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn close_does_not_panic_when_receiver_dropped() {
        let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
        let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);
        let handle = WinHandle::new(event_rx, cmd_tx, 80, 24, true);
        drop(cmd_rx);
        handle.close();
        assert!(handle.closed.load(Ordering::SeqCst));
        drop(event_tx);
    }

    #[test]
    fn send_detects_disconnect() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        drop(cmd_rx);
        assert!(!handle.closed.load(Ordering::SeqCst));
        handle.send(WinCommand::SetVisible(true));
        assert!(handle.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn close_delivers_when_channel_full() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        for _ in 0..8 {
            handle.send(WinCommand::SetVisible(true));
        }
        let rx = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                if let Ok(WinCommand::Close) = cmd_rx.try_recv() {
                    return true;
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::yield_now();
            }
        });
        handle.close();
        assert!(rx.join().unwrap());
    }

    #[test]
    fn is_disconnected_marks_closed() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        drop(cmd_rx);
        assert!(!handle.closed.load(Ordering::SeqCst));
        assert!(handle.cmd_tx.is_disconnected());
    }

    #[tokio::test]
    async fn recv_timeout_returns_timeout_event() {
        let lua = mlua::Lua::new();
        let (_event_tx, _cmd_rx, handle) = make_channels();
        lua.globals().set("win", handle).unwrap();
        let ty: String = lua
            .load("return win:recv(5).type")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(ty, "timeout");
    }

    #[tokio::test]
    async fn recv_timeout_delivers_pending_event() {
        let lua = mlua::Lua::new();
        let (event_tx, _cmd_rx, handle) = make_channels();
        event_tx
            .try_send(WinEvent::Key {
                key: "enter".into(),
            })
            .unwrap();
        lua.globals().set("win", handle).unwrap();
        let got: String = lua
            .load("local ev = win:recv(1000) return ev.type .. ':' .. ev.key")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(got, "key:enter");
    }

    #[tokio::test]
    async fn win_methods_work_while_recv_is_parked() {
        let lua = mlua::Lua::new();
        let (event_tx, cmd_rx, handle) = make_channels();
        lua.globals().set("win", handle).unwrap();
        let recv_task = tokio::spawn(
            lua.load("return win:recv(5000).type")
                .eval_async::<String>(),
        );
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        lua.load("win:set_cursor(3)").exec_async().await.unwrap();
        event_tx
            .send_async(WinEvent::Key { key: "x".into() })
            .await
            .unwrap();
        assert_eq!(recv_task.await.unwrap().unwrap(), "key");
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::SetCursor(2))));
    }
}
