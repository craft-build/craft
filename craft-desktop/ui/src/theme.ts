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
  accentText: string;
  success: string;
  danger: string;
  warning: string;
  warningDim: string;
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
