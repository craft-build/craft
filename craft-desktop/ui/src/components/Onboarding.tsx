import { useEffect, useState } from "react";
import { homeDir } from "@tauri-apps/api/path";
import { open } from "@tauri-apps/plugin-dialog";
import { useAppDispatch, useAppState } from "../state/store";
import { setMode as setSessionMode, startSession } from "../lib/acp";
import type { TabState } from "../types";

const MODES = ["build", "plan", "flow"] as const;
const MODE_LABELS: Record<string, string> = { build: "Build", plan: "Plan", flow: "Flow" };

export function Onboarding() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const t = state.tokens;
  const [cwd, setCwd] = useState("");
  const [mode, setMode] = useState<string>("build");
  const [yolo, setYolo] = useState(false);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    homeDir()
      .then(setCwd)
      .catch(() => setCwd(""));
  }, []);

  const pickDirectory = async () => {
    const picked = await open({ directory: true, defaultPath: cwd || undefined });
    if (typeof picked === "string") setCwd(picked);
  };

  const start = async () => {
    if (!cwd || starting) return;
    setStarting(true);
    setError(null);
    const tabId = crypto.randomUUID();
    try {
      const resp = await startSession(tabId, cwd, yolo);
      if (mode !== "build") {
        await setSessionMode(tabId, resp.sessionId, mode).catch(() => {});
      }
      const tab: TabState = {
        tabId,
        sessionId: resp.sessionId,
        cwd,
        title: cwd.split("/").filter(Boolean).pop() ?? cwd,
        mode,
        modes: resp.modes?.availableModes ?? [],
        configOptions: resp.configOptions ?? [],
        items: [],
        plan: [],
        todos: [],
        composerText: "",
        pending: false,
        connectionError: null,
        contextUsed: 0,
        contextSize: 0,
      };
      dispatch({ type: "START_SESSION", tab });
    } catch (e) {
      setError(String(e));
    } finally {
      setStarting(false);
    }
  };

  if (!t) {
    return (
      <div
        style={{
          flex: 1,
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          background: "#161616",
        }}
      />
    );
  }

  return (
    <div
      data-tauri-drag-region
      style={{ flex: 1, display: "flex", alignItems: "center", justifyContent: "center", background: t.bg, color: t.text }}
    >
      <div style={{ width: 460, display: "flex", flexDirection: "column", gap: 28 }}>
        <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
          <div
            style={{
              width: 40,
              height: 40,
              borderRadius: 10,
              background: t.accent,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              color: t.accentText,
              fontWeight: 700,
              fontSize: 19,
            }}
          >
            C
          </div>
          <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
            <div style={{ fontSize: 18, fontWeight: 600, letterSpacing: -0.2 }}>Craft Desktop</div>
            <div style={{ fontSize: 11, color: t.textFaint, letterSpacing: 0.4 }}>OPEN-SOURCE AGENTIC IDE</div>
          </div>
        </div>

        <div style={{ fontSize: 13.5, lineHeight: 1.6, color: t.textDim }}>
          An agent-first workspace built on the <span style={{ color: t.text }}>craft</span> coding harness &mdash;
          plan, review diffs, and approve changes without leaving the loop.
        </div>

        <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
          <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>WORKING DIRECTORY</div>
          <div
            onClick={pickDirectory}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 10,
              padding: "12px 14px",
              borderRadius: 8,
              background: t.bgInset,
              border: `1px solid ${t.border}`,
              cursor: "pointer",
            }}
          >
            <span style={{ color: t.textFaint }}>&#9656;</span>
            <span style={{ fontSize: 13, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
              {cwd || "Choose a folder…"}
            </span>
          </div>
        </div>

        <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
          <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>DEFAULT MODE</div>
          <div style={{ display: "flex", gap: 6 }}>
            {MODES.map((m) => (
              <div
                key={m}
                onClick={() => setMode(m)}
                style={{
                  flex: 1,
                  textAlign: "center",
                  padding: "9px 0",
                  borderRadius: 7,
                  fontSize: 12,
                  cursor: "pointer",
                  background: mode === m ? t.accentDim : t.bgInset,
                  color: mode === m ? t.accent : t.textDim,
                  border: `1px solid ${mode === m ? t.accent : t.border}`,
                  fontWeight: 500,
                }}
              >
                {MODE_LABELS[m]}
              </div>
            ))}
          </div>
        </div>

        <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
          <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>PERMISSIONS</div>
          <div
            onClick={() => setYolo((v) => !v)}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 10,
              padding: "12px 14px",
              borderRadius: 8,
              background: t.bgInset,
              border: `1px solid ${yolo ? t.warning : t.border}`,
              cursor: "pointer",
            }}
          >
            <span style={{ fontSize: 12, color: yolo ? t.warning : t.textDim }}>&#9881;</span>
            <span style={{ flex: 1, fontSize: 12.5, color: t.text }}>
              Yolo mode
              <span style={{ display: "block", fontSize: 11, color: t.textFaint }}>
                Auto-approve all tool permissions
              </span>
            </span>
            <span
              style={{
                width: 32,
                height: 18,
                borderRadius: 9,
                background: yolo ? t.warning : t.bg,
                border: `1px solid ${yolo ? t.warning : t.border}`,
                position: "relative",
                flex: "none",
                transition: "background 0.15s",
              }}
            >
              <span
                style={{
                  position: "absolute",
                  top: 2,
                  left: yolo ? 16 : 2,
                  width: 12,
                  height: 12,
                  borderRadius: "50%",
                  background: yolo ? t.bg : t.textFaint,
                  transition: "left 0.15s",
                }}
              />
            </span>
          </div>
        </div>

        <div
          onClick={start}
          style={{
            marginTop: 6,
            padding: "13px 0",
            borderRadius: 8,
            background: starting ? t.bgInset : t.accent,
            color: starting ? t.textFaint : t.accentText,
            textAlign: "center",
            fontSize: 13.5,
            fontWeight: 600,
            cursor: starting ? "default" : "pointer",
          }}
        >
          {starting ? "Starting…" : "Start session →"}
        </div>
        {error && <div style={{ fontSize: 11.5, color: t.danger }}>{error}</div>}

        <div
          style={{
            textAlign: "center",
            fontSize: 10.5,
            color: t.textFaint,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            gap: 6,
          }}
        >
          <span style={{ width: 6, height: 6, borderRadius: "50%", background: t.accent, display: "inline-block" }} />
          powered by craft
        </div>
      </div>
    </div>
  );
}
