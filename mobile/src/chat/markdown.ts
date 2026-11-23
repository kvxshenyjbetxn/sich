// Markdown -> sanitised HTML. Raw HTML in the source is escaped, only http(s)/mailto links are
// kept, and images are never loaded (rendered as alt text), so the output is safe to inject.
import { Marked } from "marked";

export function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

/** Returns the href when it is safe to link to, else null. */
export function safeHref(href: string | null | undefined): string | null {
  if (!href) return null;
  // Browsers ignore control chars / whitespace inside the scheme ("java\tscript:").
  // eslint-disable-next-line no-control-regex
  const compact = href.replace(/[\u0000- \u007f-\u009f]/g, "");
  return /^(https?:\/\/|mailto:)/i.test(compact) ? href.trim() : null;
}

const md = new Marked({
  gfm: true,
  breaks: false,
  async: false,
  renderer: {
    html({ text }) {
      return escapeHtml(text);
    },
    link({ href, title, tokens }) {
      const inner = this.parser.parseInline(tokens);
      const safe = safeHref(href);
      if (!safe) return inner;
      const t = title ? ` title="${escapeHtml(title)}"` : "";
      return `<a href="${escapeHtml(safe)}"${t} target="_blank" rel="noopener noreferrer">${inner}</a>`;
    },
    image({ text }) {
      return `<span class="md-img">${escapeHtml(text || "image")}</span>`;
    },
  },
});

export function renderMarkdown(src: string): string {
  try {
    return md.parse(src, { async: false });
  } catch {
    return `<p>${escapeHtml(src)}</p>`;
  }
}
