import { useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { createHighlighter, type Highlighter, type ThemeRegistration } from "shiki";
import type { ReactElement, ReactNode } from "react";
import type { Tokens } from "../theme";

const ANCHOR_RE = /\u27e6[0-9a-f]{12}\u27e7 ?/g;

export function stripAnchors(text: string): string {
  return text.replace(ANCHOR_RE, "");
}

const ENGINE_LANG_ALIASES: Record<string, string> = {
  rs: "rust",
  py: "python",
  ts: "typescript",
  js: "javascript",
  sh: "bash",
  shell: "bash",
  yml: "yaml",
  md: "markdown",
};

const SUPPORTED_LANGS = [
  "rust",
  "typescript",
  "tsx",
  "javascript",
  "jsx",
  "python",
  "go",
  "bash",
  "json",
  "toml",
  "yaml",
  "html",
  "css",
  "sql",
  "diff",
  "markdown",
] as const;

const supportedLangSet = new Set<string>(SUPPORTED_LANGS);

function resolveLang(raw: string | undefined): string | null {
  if (!raw) return null;
  const lower = raw.toLowerCase().trim();
  const resolved = ENGINE_LANG_ALIASES[lower] ?? lower;
  return supportedLangSet.has(resolved) ? resolved : null;
}

export function langFromPath(path: string): string | null {
  const base = (path.split(/[\\/]/).pop() ?? "").replace(/:\d+(-\d+)?$/, "");
  const ext = base.includes(".") ? base.split(".").pop() ?? "" : "";
  return resolveLang(ext);
}

const CRAFT_THEME_NAME = "craft-active";

let highlighterPromise: Promise<Highlighter> | null = null;

async function getHighlighter(): Promise<Highlighter> {
  if (highlighterPromise) return highlighterPromise;
  highlighterPromise = createHighlighter({
    langs: [...SUPPORTED_LANGS],
    themes: [
      {
        name: CRAFT_THEME_NAME,
        type: "dark",
        colors: { background: "#161616", foreground: "#f0f0f0" },
        tokenColors: [],
      },
    ],
  });
  return highlighterPromise;
}

function applyTheme(highlighter: Highlighter, tokens: Tokens): string {
  const themeName = CRAFT_THEME_NAME;
  const theme: ThemeRegistration = {
    name: themeName,
    type: tokens.syntaxTheme.type === "light" ? "light" : "dark",
    colors: tokens.syntaxTheme.settings,
    tokenColors: tokens.syntaxTheme.tokenColors,
  };
  try {
    highlighter.loadThemeSync(theme as never);
  } catch {
    // already loaded; the sync variant reloads in place
  }
  return themeName;
}

const renderedCache = new Map<string, string>();

export function Markdown({ md, t, streaming }: { md: string; t: Tokens; streaming?: boolean }) {
  const cleaned = useMemo(() => stripAnchors(md), [md]);

  const components = useMemo(
    () => ({
      h1: headingComponent("h1", "var(--text-lg)", t),
      h2: headingComponent("h2", "var(--text-md)", t),
      h3: headingComponent("h3", "var(--text-base)", t),
      h4: headingComponent("h4", "var(--text-base)", t),
      h5: headingComponent("h5", "var(--text-base)", t),
      h6: headingComponent("h6", "var(--text-base)", t),
      a({ href, children }: { href?: string; children?: ReactNode[] }) {
        return (
          <a href={href} style={{ color: t.accent, textDecoration: "none" }}>
            {children}
          </a>
        );
      },
      blockquote({ children }: { children?: ReactNode[] }) {
        return (
          <blockquote
            style={{
              margin: "6px 0",
              padding: "2px 12px",
              borderLeft: `3px solid ${t.border}`,
              color: t.textDim,
            }}
          >
            {children}
          </blockquote>
        );
      },
      code({ children }: { className?: string; children?: ReactNode[] }) {
        return <code style={inlineCodeStyle(t)}>{children}</code>;
      },
      p({ children }: { children?: ReactNode[] }) {
        return (
          <p style={{ margin: "3px 0", whiteSpace: "pre-wrap" }}>{children}</p>
        );
      },
      pre({ children }: { children?: ReactNode[] }) {
        const codeEl = Array.isArray(children) ? children[0] : children;
        const props = (codeEl as ReactElement<{ className?: string; children?: ReactNode[] }> | undefined)?.props;
        const text = extractText(props?.children);
        const match = /language-(\w+)/.exec(props?.className ?? "");
        if (match) {
          return <CodeBlock lang={match[1]} code={text} t={t} streaming={streaming} />;
        }
        return (
          <pre
            style={{
              fontFamily: "var(--font-mono)",
              fontSize: 11.5,
              lineHeight: 1.5,
              background: t.bgInset,
              border: `1px solid ${t.border}`,
              borderRadius: "var(--radius-md)",
              padding: "10px 12px",
              margin: "6px 0",
              overflow: "auto",
            }}
            className="cd-scroll"
          >
            <code>{text}</code>
          </pre>
        );
      },
    }),
    [t, streaming],
  );

  return (
    <div
      className="cd-markdown"
      style={{
        fontSize: 13,
        lineHeight: 1.6,
        color: t.text,
        wordBreak: "break-word",
        whiteSpace: "pre-wrap",
      }}
    >
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components as never}>
        {cleaned}
      </ReactMarkdown>
    </div>
  );
}

type HeadingTag = "h1" | "h2" | "h3" | "h4" | "h5" | "h6";

function headingComponent(tag: HeadingTag, fontSize: string, t: Tokens) {
  return function Heading({ children }: { children?: ReactNode[] }) {
    const Tag = tag;
    return (
      <Tag
        style={{
          fontFamily: "var(--font-display)",
          fontWeight: "var(--weight-semibold)",
          letterSpacing: "var(--tracking-tight)",
          fontSize,
          color: t.text,
          margin: "10px 0 4px",
        }}
      >
        {children}
      </Tag>
    );
  };
}

function extractText(node: ReactNode | ReactNode[] | undefined): string {
  if (node == null || node === false) return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (typeof node === "object" && "props" in node) {
    return extractText((node as { props?: { children?: ReactNode } }).props?.children);
  }
  return "";
}

function inlineCodeStyle(t: Tokens): React.CSSProperties {
  return {
    fontFamily: "var(--font-mono)",
    fontSize: "0.88em",
    background: t.bgInset,
    padding: "1px 5px",
    borderRadius: "var(--radius-xs)",
    border: `1px solid ${t.border}`,
  };
}

const CodeBlock = ({
  lang,
  code,
  t,
  streaming,
}: {
  lang: string;
  code: string;
  t: Tokens;
  streaming?: boolean;
}) => {
  const resolved = resolveLang(lang);
  const [html, setHtml] = useState<string | null>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const codeRef = useRef(code);
  codeRef.current = code;

  useEffect(() => {
    if (!resolved) return;

    if (streaming) {
      if (debounceRef.current) clearTimeout(debounceRef.current);
      const timer = setTimeout(() => {
        runHighlight(resolved, codeRef.current, t).then(setHtml);
      }, 250);
      debounceRef.current = timer;
      return () => {
        if (debounceRef.current) clearTimeout(debounceRef.current);
      };
    }

    let cancelled = false;
    runHighlight(resolved, code, t).then((result) => {
      if (!cancelled) setHtml(result);
    });
    return () => {
      cancelled = true;
    };
  }, [resolved, code, t, streaming]);

  const bg = t.syntaxTheme.settings.background ?? t.bgInset;

  if (!resolved || html === null) {
    return (
      <pre
        style={{
          fontFamily: "var(--font-mono)",
          fontSize: 11.5,
          lineHeight: 1.5,
          background: bg,
          border: `1px solid ${t.border}`,
          borderRadius: "var(--radius-md)",
          padding: "10px 12px",
          margin: "6px 0",
          overflow: "auto",
        }}
        className="cd-scroll"
      >
        <code>{code}</code>
      </pre>
    );
  }

  return (
    <pre
      style={{
        fontFamily: "var(--font-mono)",
        fontSize: 11.5,
        lineHeight: 1.5,
        background: bg,
        border: `1px solid ${t.border}`,
        borderRadius: "var(--radius-md)",
        padding: "10px 12px",
        margin: "6px 0",
        overflow: "auto",
      }}
      className="cd-scroll"
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
};

async function runHighlight(lang: string, code: string, t: Tokens): Promise<string | null> {
  const cacheKey = `${t.name}:${lang}:${simpleHash(code)}:${code.length}`;
  const cached = renderedCache.get(cacheKey);
  if (cached !== undefined) return cached;

  try {
    const highlighter = await getHighlighter();
    const themeName = applyTheme(highlighter, t);
    const result = highlighter.codeToHtml(code, {
      lang,
      theme: themeName as never,
    });
    renderedCache.set(cacheKey, result);
    return result;
  } catch (e) {
    console.error("shiki highlight failed", e);
    return null;
  }
}

function simpleHash(s: string): string {
  let h = 0;
  for (let i = 0; i < s.length; i++) {
    h = ((h << 5) - h + s.charCodeAt(i)) | 0;
  }
  return Math.abs(h).toString(36);
}
