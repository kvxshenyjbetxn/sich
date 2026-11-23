import { memo, useEffect, useMemo, useRef, useState } from "react";
import { Bot, ChevronRight, FilePen, FileText, Globe, ListChecks, LoaderCircle, Search, SquareTerminal, Wrench, ImageOff } from "lucide-react";
import { useI18n } from "../ctx";
import type { TermRef } from "../lib/types";
import { fetchChatImage, imageQueue } from "./chatApi";
import { resultPreview, toolIcon, toolSummaryLine, type ChatItem, type ToolIcon } from "./messages";
import { renderMarkdown } from "./markdown";
import { termKey } from "../lib/types";

const ICONS: Record<ToolIcon, typeof Wrench> = {
  terminal: SquareTerminal,
  file: FileText,
  edit: FilePen,
  search: Search,
  globe: Globe,
  list: ListChecks,
  bot: Bot,
  wrench: Wrench,
};

/** Lazily fetched thumbnail (at most 2 requests at a time, see `imageQueue`). */
export function ChatImage({ term, imageRef, alt, onOpen }: { term: TermRef; imageRef: string; alt: string; onOpen: (src: string, alt: string) => void }) {
  const { t } = useI18n();
  const host = useRef<HTMLButtonElement>(null);
  const cacheKey = `${termKey(term)}|${imageRef}`;
  const [src, setSrc] = useState<string | null>(() => imageQueue.cached(cacheKey) ?? null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    if (src) return;
    let alive = true;
    const go = () =>
      fetchChatImage(term, imageRef).then(
        (u) => alive && setSrc(u),
        () => alive && setFailed(true),
      );
    const el = host.current;
    if (!el || typeof IntersectionObserver === "undefined") {
      void go();
      return () => {
        alive = false;
      };
    }
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        io.disconnect();
        void go();
      }
    });
    io.observe(el);
    return () => {
      alive = false;
      io.disconnect();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cacheKey, src]);

  return (
    <button ref={host} className="cm-thumb" disabled={!src} onClick={() => src && onOpen(src, alt)} title={alt}>
      {src ? <img src={src} alt={alt} /> : failed ? <span className="cm-thumb-ph err" title={t("chat_image_failed")}><ImageOff size={20} /></span> : <span className="cm-thumb-ph"><LoaderCircle size={18} className="spin" /></span>}
    </button>
  );
}

function Markdown({ source }: { source: string }) {
  const html = useMemo(() => renderMarkdown(source), [source]);
  return (
    <div
      className="cm-md"
      onClick={(e) => {
        const a = (e.target as HTMLElement).closest("a");
        if (!a) return;
        // Never navigate the app WebView away; open links in the system browser instead.
        e.preventDefault();
        const href = a.getAttribute("href");
        if (!href) return;
        try {
          window.open(href, "_blank", "noopener,noreferrer");
        } catch {
          /* no handler available */
        }
      }}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}

interface Props {
  item: ChatItem;
  term: TermRef;
  expanded: boolean;
  onToggle: (key: string) => void;
  onOpenImage: (src: string, alt: string) => void;
}

export const ChatItemView = memo(function ChatItemView({ item, term, expanded, onToggle, onOpenImage }: Props) {
  const { t } = useI18n();
  switch (item.kind) {
    case "user":
      return (
        <div className={`cm-row user${item.pending ? " pending" : ""}`}>
          <div className="cm-bubble">
            {item.text && <div className="cm-user-text">{item.text}</div>}
            {item.images.length > 0 && (
              <div className="cm-thumbs">
                {item.images.map((im, i) => (
                  <ChatImage key={`${im.ref}:${i}`} term={term} imageRef={im.ref} alt={im.alt} onOpen={onOpenImage} />
                ))}
              </div>
            )}
          </div>
        </div>
      );
    case "assistant":
      return (
        <div className="cm-row assistant">
          <Markdown source={item.markdown} />
        </div>
      );
    case "reasoning":
      return (
        <details className="cm-reasoning">
          <summary>{t("chat_reasoning")}</summary>
          <div className="cm-reasoning-text">{item.text}</div>
        </details>
      );
    case "system":
      return <div className="cm-system">{item.text}</div>;
    case "image":
      return (
        <div className="cm-row assistant">
          <ChatImage term={term} imageRef={item.ref} alt={item.alt} onOpen={onOpenImage} />
        </div>
      );
    case "tool": {
      const Icon = ICONS[toolIcon(item.name)];
      const err = item.result?.isError ?? false;
      return (
        <div className={`cm-tool${err ? " err" : ""}${expanded ? " open" : ""}`}>
          <button className="cm-tool-row" onClick={() => onToggle(item.key)}>
            <ChevronRight size={14} className="cm-chev" />
            <Icon size={14} />
            <span className="cm-tool-name">{item.name || "tool"}</span>
            <span className="cm-tool-sum ellipsis">
              {item.summary ? toolSummaryLine(item.summary) : item.result ? resultPreview(item.result.output) : ""}
            </span>
            {!item.result && <LoaderCircle size={12} className="spin" />}
            {err && <span className="cm-tool-badge">{t("chat_tool_error")}</span>}
          </button>
          {expanded && (
            <div className="cm-tool-body">
              {item.summary && <pre className="cm-pre">{item.summary}</pre>}
              <pre className="cm-pre result">{item.result ? item.result.output || t("chat_tool_no_output") : t("chat_tool_running")}</pre>
            </div>
          )}
        </div>
      );
    }
  }
});
