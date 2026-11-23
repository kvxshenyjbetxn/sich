import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { LoaderCircle, Paperclip, SendHorizontal, X, TriangleAlert } from "lucide-react";
import { api } from "../api";
import { useI18n } from "../ctx";
import { prepareImage } from "../imageUpload";

interface Attachment {
  key: number;
  name: string;
  preview: string | null;
  status: "uploading" | "done" | "error";
  uploadId?: string;
  error?: string;
}

interface Props {
  disabled: boolean;
  onSend: (text: string, imageIds: string[]) => void;
}

const MAX_ROWS_PX = 140;

function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  try {
    return JSON.stringify(e);
  } catch {
    return String(e);
  }
}

export function Composer({ disabled, onSend }: Props) {
  const { t } = useI18n();
  const [text, setText] = useState("");
  const [items, setItems] = useState<Attachment[]>([]);
  const area = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const nextKey = useRef(1);
  const previews = useRef(new Set<string>());

  // Auto-grow the textarea up to a few lines.
  useLayoutEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_ROWS_PX)}px`;
  }, [text]);

  useEffect(() => {
    const set = previews.current;
    return () => set.forEach((u) => URL.revokeObjectURL(u));
  }, []);

  const patch = (key: number, p: Partial<Attachment>) => setItems((cur) => cur.map((a) => (a.key === key ? { ...a, ...p } : a)));

  const addFiles = async (files: FileList | null) => {
    if (!files) return;
    for (const file of Array.from(files)) {
      const key = nextKey.current++;
      setItems((cur) => [...cur, { key, name: file.name, preview: null, status: "uploading" }]);
      try {
        const prepared = await prepareImage(file);
        const preview = URL.createObjectURL(prepared.blob);
        previews.current.add(preview);
        patch(key, { preview, name: prepared.name });
        const bytes = new Uint8Array(await prepared.blob.arrayBuffer());
        const id = await api.uploadImage(prepared.name, bytes);
        patch(key, { status: "done", uploadId: id });
      } catch (e) {
        console.error("image upload failed", e);
        patch(key, { status: "error", error: errorText(e) });
      }
    }
    if (fileInput.current) fileInput.current.value = "";
  };

  const remove = (key: number) =>
    setItems((cur) => {
      const gone = cur.find((a) => a.key === key);
      if (gone?.preview) {
        URL.revokeObjectURL(gone.preview);
        previews.current.delete(gone.preview);
      }
      return cur.filter((a) => a.key !== key);
    });

  const failed = items.find((a) => a.status === "error");
  const uploading = items.some((a) => a.status === "uploading");
  const ready = items.filter((a) => a.status === "done" && a.uploadId).map((a) => a.uploadId!);
  const canSend = !disabled && !uploading && (text.trim().length > 0 || ready.length > 0);

  const send = () => {
    if (!canSend) return;
    onSend(text, ready);
    setText("");
    items.forEach((a) => a.preview && URL.revokeObjectURL(a.preview));
    setItems([]);
  };

  return (
    <div className="composer">
      {items.length > 0 && (
        <div className="thumbs">
          {items.map((a) => (
            <div key={a.key} className={`thumb ${a.status}`} title={a.status === "error" ? `${t("upload_failed")}: ${a.error ?? ""}` : a.name}>
              {a.preview && <img src={a.preview} alt={a.name} />}
              {a.status === "uploading" && (
                <span className="thumb-overlay">
                  <LoaderCircle size={18} className="spin" />
                </span>
              )}
              {a.status === "error" && (
                <span className="thumb-overlay err">
                  <TriangleAlert size={18} />
                </span>
              )}
              <button className="thumb-x" aria-label={t("remove")} onClick={() => remove(a.key)}>
                <X size={12} />
              </button>
            </div>
          ))}
        </div>
      )}
      {failed && (
        <div className="upload-error">
          {t("upload_failed")}: {failed.error}
        </div>
      )}
      <div className="composer-row">
        <input ref={fileInput} type="file" accept="image/*" multiple hidden onChange={(e) => void addFiles(e.target.files)} />
        <button className="icon-btn" aria-label={t("attach_image")} disabled={disabled} onClick={() => fileInput.current?.click()}>
          <Paperclip size={20} />
        </button>
        <textarea
          ref={area}
          rows={1}
          value={text}
          placeholder={t("composer_placeholder")}
          onChange={(e) => setText(e.target.value)}
          autoCapitalize="sentences"
          spellCheck={false}
        />
        <button className="send-btn" aria-label={t("send")} disabled={!canSend} onClick={send}>
          <SendHorizontal size={18} />
        </button>
      </div>
    </div>
  );
}
