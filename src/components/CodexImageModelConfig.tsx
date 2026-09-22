import { useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import { useTranslation } from "react-i18next";
import { SingleSelectDropdown } from "./SingleSelectDropdown";
import "./CodexLocalAccessModal.css";

const PRESETS = ["gpt-image-2.5", "gpt-image-2"];
const CUSTOM = "__custom__";
const keyPrefix = "codex.localAccess.imageGenerationModel";

interface Props {
  model?: string;
  disabled: boolean;
  onSave: (model: string) => Promise<unknown> | unknown;
}

/**
 * 紧凑版生图模型选择：与「选择生图账号」同排，选中预设立即保存；
 * 选择「自定义」时才展开输入框与保存按钮，避免占用一整个卡片的高度。
 */
export function CodexImageModelSelect({ model, disabled, onSave }: Props) {
  const { t } = useTranslation();
  const initialModel = model?.trim() || PRESETS[0];
  const [selection, setSelection] = useState(
    PRESETS.includes(initialModel) ? initialModel : CUSTOM,
  );
  const [customModel, setCustomModel] = useState(
    PRESETS.includes(initialModel) ? "" : initialModel,
  );
  const [customOpen, setCustomOpen] = useState(!PRESETS.includes(initialModel));
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const savingRef = useRef(false);
  const busy = disabled || pending;

  useEffect(() => {
    const next = model?.trim() || PRESETS[0];
    const isPreset = PRESETS.includes(next);
    setSelection(isPreset ? next : CUSTOM);
    setCustomModel(isPreset ? "" : next);
    setCustomOpen(!isPreset);
  }, [model]);

  const save = async (nextModel: string) => {
    if (busy || savingRef.current) return;
    const normalized = nextModel.trim();
    if (!normalized) {
      setError(t(`${keyPrefix}.required`));
      return;
    }
    savingRef.current = true;
    setPending(true);
    setError("");
    try {
      await onSave(normalized);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      savingRef.current = false;
      setPending(false);
    }
  };

  return (
    <div className="codex-image-model-inline">
      <span className="codex-image-model-inline-label">
        {t(`${keyPrefix}.label`)}
      </span>
      <SingleSelectDropdown
        value={selection}
        options={[
          ...PRESETS.map((value) => ({ value, label: value })),
          { value: CUSTOM, label: t(`${keyPrefix}.custom`) },
        ]}
        className="codex-image-model-inline-select"
        menuWidth={190}
        menuMaxHeight={200}
        disabled={busy}
        ariaLabel={t(`${keyPrefix}.label`)}
        onChange={(value) => {
          setError("");
          setSelection(value);
          if (value === CUSTOM) {
            setCustomOpen(true);
            return;
          }
          setCustomOpen(false);
          void save(value);
        }}
      />
      {customOpen && (
        <>
          <input
            className="codex-image-model-inline-input"
            type="text"
            value={customModel}
            maxLength={200}
            placeholder={t(`${keyPrefix}.placeholder`)}
            aria-label={t(`${keyPrefix}.placeholder`)}
            aria-invalid={Boolean(error)}
            disabled={busy}
            onChange={(event) => {
              setCustomModel(event.target.value);
              setError("");
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.nativeEvent.isComposing) {
                event.preventDefault();
                void save(customModel);
              }
            }}
          />
          <button
            type="button"
            className="btn btn-secondary btn-sm"
            onClick={() => void save(customModel)}
            disabled={busy || !customModel.trim()}
          >
            {pending && <RefreshCw size={14} className="loading-spinner" />}
            {t(`${keyPrefix}.save`)}
          </button>
        </>
      )}
      {error && (
        <small role="alert" className="codex-local-access-image-model-error">
          {error}
        </small>
      )}
    </div>
  );
}
