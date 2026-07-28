export interface ModeColors {
  build: string;
  plan: string;
  flow: string;
}

export interface ShikiTokenColor {
  scope: string;
  settings: { foreground: string; fontStyle?: string };
}

export interface ShikiTheme {
  name: string;
  type: string;
  settings: Record<string, string>;
  tokenColors: ShikiTokenColor[];
}

export interface Tokens {
  name: string;
  label: string;
  bg: string;
  bgElevated: string;
  bgInset: string;
  border: string;
  text: string;
  textDim: string;
  textFaint: string;
  accent: string;
  accentDim: string;
  accentDim2: string;
  accentSecondary: string;
  accentTertiary: string;
  accentText: string;
  success: string;
  danger: string;
  warning: string;
  warningDim: string;
  info: string;
  modeColors: ModeColors;
  syntaxTheme: ShikiTheme;
}

export interface ThemeName {
  name: string;
  label: string;
}

export function modeColor(mode: string, t: Tokens): string {
  switch (mode) {
    case "build":
      return t.modeColors.build;
    case "plan":
      return t.modeColors.plan;
    case "flow":
      return t.modeColors.flow;
    default:
      return t.textFaint;
  }
}

// A darker recess than `bg` for terminal-style blocks (tool output,
// permission commands) — `bg` is already the darkest surface token the
// backend emits, so we shade further rather than adding a new server token.
export function terminalBg(t: Tokens): string {
  return `color-mix(in oklch, ${t.bg} 90%, black)`;
}

// Brand gradient (blue -> violet -> magenta) for primary buttons, switches,
// and progress fills. `accentSecondary`/`accentTertiary` fall back to flat
// `accent` for themes that don't define them, so this degrades to a solid
// color rather than picking up unrelated hues on non-craft themes.
export function brandGradient(t: Tokens): string {
  return `linear-gradient(135deg, ${t.accent} 0%, ${t.accentSecondary} 55%, ${t.accentTertiary} 100%)`;
}

// Soft (16%-alpha) wash version of brandGradient, used for selected/active
// row backgrounds (command palette, pill selectors).
export function brandGradientSoft(t: Tokens): string {
  const wash = (c: string) => `color-mix(in oklch, ${c} 16%, transparent)`;
  return `linear-gradient(135deg, ${wash(t.accent)} 0%, ${wash(t.accentSecondary)} 55%, ${wash(t.accentTertiary)} 100%)`;
}

// 16%-alpha wash of a mode's color, used for the AGENT mode pill buttons.
export function modeColorWash(mode: string, t: Tokens): string {
  return `color-mix(in oklch, ${modeColor(mode, t)} 16%, transparent)`;
}

export function modeLabel(mode: string): string {
  switch (mode) {
    case "build":
      return "Build";
    case "plan":
      return "Plan";
    case "flow":
      return "Flow";
    default:
      return mode;
  }
}
