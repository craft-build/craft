import { useEffect } from "react";
import { useAppDispatch, useAppState } from "./state/store";
import { onClosed, onPermissionRequest, onPromptDone, onQuestion, onSessionUpdate } from "./lib/acp";
import type { PermissionOption, QuestionSpec, SessionUpdate } from "./types";
import { Onboarding } from "./components/Onboarding";
import { Shell } from "./components/Shell";

export default function App() {
  const state = useAppState();
  const dispatch = useAppDispatch();
  const t = state.tokens;

  useEffect(() => {
    const unlistenPromises = [
      onSessionUpdate(({ tabId, update }) => {
        dispatch({ type: "SESSION_UPDATE", tabId, update: update as unknown as SessionUpdate });
      }),
      onPermissionRequest(({ tabId, requestId, params }) => {
        const toolTitle = params?.toolCall?.title ?? "Permission requested";
        const options = (params?.options ?? []) as unknown as PermissionOption[];
        dispatch({ type: "PERMISSION_REQUEST", tabId, requestId, title: toolTitle, options });
      }),
      onQuestion(({ tabId, requestId, params }) => {
        const questions = (params?.questions ?? []) as unknown as QuestionSpec[];
        dispatch({ type: "QUESTION_REQUEST", tabId, requestId, questions });
      }),
      onPromptDone(({ tabId, ok, error }) => {
        if (ok) {
          dispatch({ type: "PROMPT_DONE", tabId });
        } else {
          dispatch({ type: "CONNECTION_ERROR", tabId, message: error ?? "Prompt failed" });
        }
      }),
      onClosed(({ tabId }) => {
        dispatch({ type: "CONNECTION_CLOSED", tabId });
      }),
    ];
    return () => {
      unlistenPromises.forEach((p) => p.then((unlisten) => unlisten()));
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (!t) {
    return (
      <div
        style={{
          width: "100vw",
          height: "100vh",
          background: "#161616",
          fontFamily: "'JetBrains Mono', ui-monospace, Menlo, monospace",
        }}
      />
    );
  }

  return (
    <div
      style={{
        width: "100vw",
        height: "100vh",
        overflow: "hidden",
        fontFamily: "'JetBrains Mono', ui-monospace, Menlo, monospace",
        display: "flex",
        flexDirection: "column",
        position: "relative",
        background: t.bg,
        color: t.text,
      }}
    >
      {state.screen === "onboarding" ? <Onboarding /> : <Shell />}
    </div>
  );
}
