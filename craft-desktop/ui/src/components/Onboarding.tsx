import { useEffect, useState } from "react";
import { homeDir } from "@tauri-apps/api/path";
import { open } from "@tauri-apps/plugin-dialog";
import { useAppDispatch, useAppState } from "../state/store";
import { setConfigOption, setMode as setSessionMode, startSession } from "../lib/acp";
import { brandGradient, brandGradientSoft, modeColor, modeColorWash } from "../theme";
import type { SshTarget, TabState } from "../types";
import foxMark from "../assets/mark-fox.png";

// Drift note: this list is duplicated from the server's session_modes table
// (craft-acp/src/methods.rs `session_modes` / `new_session_response`). It is
// only used for the pre-session default-mode picker, before the server has
// returned `availableModes`. If a mode is added server-side, add it here too.
const MODES = ["build", "plan", "flow"] as const;
const MODE_LABELS: Record<string, string> = { build: "Build", plan: "Plan", flow: "Flow" };

export function Onboarding() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const t = state.tokens;
  const [cwd, setCwd] = useState("");
  const [transport, setTransport] = useState<"local" | "ssh">("local");
  const [host, setHost] = useState("");
  const [remoteCwd, setRemoteCwd] = useState("");
  const [remoteCraft, setRemoteCraft] = useState("");
  const [mode, setMode] = useState<string>("build");
  const [yolo, setYolo] = useState(false);
  const [autoReview, setAutoReview] = useState(false);
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
    if (starting) return;
    if (transport === "local" && !cwd) return;
    if (transport === "ssh" && (!host || !remoteCwd || !remoteCwd.startsWith("/"))) return;
    setStarting(true);
    setError(null);
    const tabId = crypto.randomUUID();
    try {
      const ssh: SshTarget | null =
        transport === "ssh" ? { host, remoteCraft: remoteCraft.trim() || undefined } : null;
      const effectiveCwd = transport === "local" ? cwd : remoteCwd;
      const resp = await startSession(tabId, effectiveCwd, yolo, ssh);
      if (mode !== "build") {
        await setSessionMode(tabId, resp.sessionId, mode).catch(() => {});
      }
      if (autoReview) {
        await setConfigOption(tabId, resp.sessionId, "auto_review", "true").catch(() => {});
      }
      const basename = effectiveCwd.split("/").filter(Boolean).pop() ?? effectiveCwd;
      const title = ssh ? `${ssh.host}:${basename}` : basename;
      const tab: TabState = {
        tabId,
        sessionId: resp.sessionId,
        cwd: effectiveCwd,
        title,
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
        sessionCost: 0,
        ssh,
        commands: null,
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
          background: "#060911",
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
          <img src={foxMark} alt="" style={{ width: 34, height: 34, objectFit: "contain", flex: "none" }} />
          <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
            <div style={{ fontFamily: "var(--font-display)", fontSize: 18, fontWeight: 600, letterSpacing: -0.2 }}>Craft Desktop</div>
            <div style={{ fontSize: 11, color: t.textFaint, letterSpacing: 0.4 }}>OPEN-SOURCE AGENTIC IDE</div>
          </div>
        </div>

        <div style={{ fontSize: 13.5, lineHeight: 1.6, color: t.textDim }}>
          An agent-first workspace built on the <span style={{ color: t.text }}>craft</span> coding harness &mdash;
          plan, review diffs, and approve changes without leaving the loop.
        </div>

        <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
          <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>TRANSPORT</div>
          <div style={{ display: "flex", gap: 6 }}>
            {(["local", "ssh"] as const).map((tr) => (
              <div
                key={tr}
                onClick={() => setTransport(tr)}
                style={{
                  flex: 1,
                  textAlign: "center",
                  padding: "9px 0",
                  fontSize: 12,
                  cursor: "pointer",
                  borderRadius: "var(--radius-sm)",
                  background: transport === tr ? brandGradientSoft(t) : t.bgInset,
                  color: transport === tr ? t.accentSecondary : t.textDim,
                  border: `1px solid ${transport === tr ? t.accentSecondary : t.border}`,
                  fontWeight: 500,
                }}
              >
                {tr === "local" ? "Local" : "SSH"}
              </div>
            ))}
          </div>
        </div>

        {transport === "local" ? (
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>WORKING DIRECTORY</div>
            <div
              onClick={pickDirectory}
              style={{
                display: "flex",
                alignItems: "center",
                gap: 10,
                padding: "11px 13px",
                background: t.bgInset,
                border: `1px solid ${t.border}`,
                borderRadius: "var(--radius-sm)",
                cursor: "pointer",
              }}
            >
              <span style={{ color: t.textFaint }}>&#10095;</span>
              <span style={{ fontSize: 13, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
                {cwd || "Choose a folder…"}
              </span>
            </div>
          </div>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
            <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
              <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>HOST</div>
              <input
                value={host}
                onChange={(e) => setHost(e.target.value)}
                placeholder="user@host"
                className="cd-input"
                style={{
                  padding: "11px 13px",
                  background: t.bgInset,
                  border: `1px solid ${t.border}`,
                  borderRadius: "var(--radius-sm)",
                  fontSize: 13,
                  color: t.text,
                  outline: "none",
                  width: "100%",
                  boxSizing: "border-box",
                  ["--ring-color" as string]: t.accent,
                } as React.CSSProperties}
              />
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
              <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>REMOTE PATH</div>
              <input
                value={remoteCwd}
                onChange={(e) => setRemoteCwd(e.target.value)}
                placeholder="/home/user/project"
                className="cd-input"
                style={{
                  padding: "11px 13px",
                  background: t.bgInset,
                  border: `1px solid ${remoteCwd && !remoteCwd.startsWith("/") ? t.danger : t.border}`,
                  borderRadius: "var(--radius-sm)",
                  fontSize: 13,
                  color: t.text,
                  outline: "none",
                  width: "100%",
                  boxSizing: "border-box",
                  ["--ring-color" as string]: t.accent,
                } as React.CSSProperties}
              />
              {remoteCwd && !remoteCwd.startsWith("/") && (
                <div style={{ fontSize: 11, color: t.danger }}>Remote path must start with /</div>
              )}
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
              <div style={{ fontSize: 10.5, color: t.textFaint, letterSpacing: 0.5 }}>REMOTE CRAFT PATH (OPTIONAL)</div>
              <input
                value={remoteCraft}
                onChange={(e) => setRemoteCraft(e.target.value)}
                placeholder="craft"
                className="cd-input"
                style={{
                  padding: "11px 13px",
                  background: t.bgInset,
                  border: `1px solid ${t.border}`,
                  borderRadius: "var(--radius-sm)",
                  fontSize: 13,
                  color: t.text,
                  outline: "none",
                  width: "100%",
                  boxSizing: "border-box",
                  ["--ring-color" as string]: t.accent,
                } as React.CSSProperties}
              />
              <div style={{ fontSize: 10.5, color: t.textFaint, lineHeight: 1.5 }}>
                Leave blank to use craft on the remote PATH. Requires key-based (non-interactive) auth and the host key already accepted.
              </div>
            </div>
          </div>
        )}

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
                  fontSize: 12,
                  cursor: "pointer",
                  borderRadius: "var(--radius-sm)",
                  background: mode === m ? modeColorWash(m, t) : t.bgInset,
                  color: mode === m ? modeColor(m, t) : t.textDim,
                  border: `1px solid ${mode === m ? modeColor(m, t) : t.border}`,
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
              padding: "11px 13px",
              background: t.bgInset,
              border: `1px solid ${yolo ? t.warning : t.border}`,
              borderRadius: "var(--radius-sm)",
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
                borderRadius: "var(--radius-pill)",
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
          <div
            onClick={() => setAutoReview((v) => !v)}
            style={{
              display: "flex",
              alignItems: "center",
              gap: 10,
              padding: "11px 13px",
              background: t.bgInset,
              border: `1px solid ${autoReview ? t.accent : t.border}`,
              borderRadius: "var(--radius-sm)",
              cursor: "pointer",
            }}
          >
            <span style={{ fontSize: 12, color: autoReview ? t.accent : t.textDim }}>&#129504;</span>
            <span style={{ flex: 1, fontSize: 12.5, color: t.text }}>
              Auto-review
              <span style={{ display: "block", fontSize: 11, color: t.textFaint }}>
                Let an LLM reviewer allow or deny tool calls
              </span>
            </span>
            <span
              style={{
                width: 32,
                height: 18,
                borderRadius: "var(--radius-pill)",
                background: autoReview ? t.accent : t.bg,
                border: `1px solid ${autoReview ? t.accent : t.border}`,
                position: "relative",
                flex: "none",
                transition: "background 0.15s",
              }}
            >
              <span
                style={{
                  position: "absolute",
                  top: 2,
                  left: autoReview ? 16 : 2,
                  width: 12,
                  height: 12,
                  borderRadius: "50%",
                  background: autoReview ? t.bg : t.textFaint,
                  transition: "left 0.15s",
                }}
              />
            </span>
          </div>
        </div>

        <div
          onClick={start}
          className={starting ? undefined : "cd-btn-primary"}
          style={{
            marginTop: 6,
            padding: "13px 0",
            borderRadius: "var(--radius-sm)",
            background: starting ? t.bgInset : brandGradient(t),
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
