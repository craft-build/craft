import type { Tokens } from "../theme";
import type { TabState } from "../types";

const STATUS_LABEL: Record<string, string> = {
  pending: "PENDING",
  in_progress: "ACTIVE",
  completed: "DONE",
};

export function PlanGraph({ tab, t }: { tab: TabState; t: Tokens }) {
  const statusColor = (status: string) =>
    status === "completed" ? t.success : status === "in_progress" ? t.accent : t.textFaint;

  return (
    <div style={{ flex: 1, overflow: "auto", padding: "26px 30px" }} className="cd-scroll">
      <div style={{ fontSize: 12, color: t.textFaint, marginBottom: 18 }}>
        Plan for &ldquo;{tab.title}&rdquo; &middot; {tab.mode} mode
      </div>
      {tab.plan.length === 0 ? (
        <div style={{ fontSize: 12.5, color: t.textFaint, lineHeight: 1.6 }}>
          No plan yet. Switch to <span style={{ color: t.textDim }}>Flow</span> mode and send a request &mdash; craft
          reports its stage-by-stage execution plan here as it runs.
        </div>
      ) : (
        <div style={{ position: "relative", display: "flex", flexDirection: "column", gap: 22, paddingLeft: 26 }}>
          <div style={{ position: "absolute", left: 5, top: 6, bottom: 6, width: 2, background: t.border }} />
          {tab.plan.map((stage, i) => (
            <div key={i} style={{ position: "relative" }}>
              <div
                style={{
                  position: "absolute",
                  left: -26,
                  top: 4,
                  width: 11,
                  height: 11,
                  borderRadius: "50%",
                  background: statusColor(stage.status),
                  border: `2px solid ${t.bg}`,
                }}
              />
              <div
                style={{
                  padding: "13px 15px",
                  borderRadius: 9,
                  background: t.bgInset,
                  border: `1px solid ${stage.status === "in_progress" ? t.accent : t.border}`,
                  display: "flex",
                  flexDirection: "column",
                  gap: 8,
                }}
              >
                <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
                  <div style={{ fontSize: 12.5, color: t.text, fontWeight: 600 }}>{stage.content}</div>
                  <div style={{ fontSize: 10, letterSpacing: 0.3, color: statusColor(stage.status) }}>
                    {STATUS_LABEL[stage.status] ?? stage.status.toUpperCase()}
                  </div>
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
