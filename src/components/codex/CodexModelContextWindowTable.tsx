import { useTranslation } from "react-i18next";
import { SingleSelectDropdown } from "../SingleSelectDropdown";

interface CodexModelContextWindowTableProps {
  models: string[];
  drafts: Record<string, string>;
  onChange: (model: string, value: string) => void;
  /** 逐模型识图开关；不传则不渲染该列。 */
  visionStates?: Record<string, boolean>;
  onVisionChange?: (model: string, value: boolean) => void;
  disabled?: boolean;
}

/** 与 Codex「上下文与压缩阈值」保持一致的上下文预设。 */
const CONTEXT_PRESETS = {
  preset_516k: "516000",
  preset_1m: "1000000",
} as const;

function resolveContextPreset(value: string): string {
  const normalized = value.trim();
  if (!normalized) return "official";
  if (normalized === CONTEXT_PRESETS.preset_516k) return "preset_516k";
  if (normalized === CONTEXT_PRESETS.preset_1m) return "preset_1m";
  return "custom";
}

export function CodexModelContextWindowTable({
  models,
  drafts,
  onChange,
  visionStates,
  onVisionChange,
  disabled = false,
}: CodexModelContextWindowTableProps) {
  const { t } = useTranslation();
  if (models.length === 0) return null;
  const showVision = typeof onVisionChange === "function";
  const visionLabel = t("codex.modelProviders.vision.allModels", "图片输入");
  const presetOptions = [
    {
      value: "official",
      label: t("codex.contextOverride.followOfficial", "跟随官方"),
    },
    { value: "preset_516k", label: "516K" },
    { value: "preset_1m", label: "1M" },
    { value: "custom", label: t("codex.api.modelCatalog.contextCustom", "自定义") },
  ];

  return (
    <div
      className={`api-model-context-window-panel ${
        showVision ? "with-vision" : ""
      }`}
    >
      <div className="api-model-context-window-head">
        <span>{t("codex.api.modelCatalog.modelColumn", "模型")}</span>
        <span>
          {t("codex.api.modelCatalog.contextWindow", "上下文")}
          <em className="api-model-context-window-optional">
            {t("codex.api.modelCatalog.contextWindowOptional", "可选")}
          </em>
        </span>
        {showVision ? <span>{visionLabel}</span> : null}
      </div>
      <div className="api-model-context-window-rows">
        {models.map((model) => {
          const value = drafts[model] ?? "";
          const preset = resolveContextPreset(value);
          return (
            <div key={model} className="api-model-context-window-row">
              <span title={model}>{model}</span>
              <div className="api-model-context-window-preset">
                <SingleSelectDropdown
                  value={preset}
                  options={presetOptions}
                  onChange={(next) => {
                    if (next === "official") return onChange(model, "");
                    if (next === "preset_516k") {
                      return onChange(model, CONTEXT_PRESETS.preset_516k);
                    }
                    if (next === "preset_1m") {
                      return onChange(model, CONTEXT_PRESETS.preset_1m);
                    }
                    return onChange(model, value.trim());
                  }}
                  className="api-model-context-window-select"
                  disabled={disabled}
                  ariaLabel={`${model} ${t(
                    "codex.api.modelCatalog.contextWindow",
                    "上下文",
                  )}`}
                  menuPlacement="up"
                />
                {preset === "custom" ? (
                  <input
                    type="text"
                    inputMode="numeric"
                    className="form-input"
                    value={value}
                    onChange={(event) => onChange(model, event.target.value)}
                    disabled={disabled}
                    aria-label={`${model} ${t(
                      "codex.api.modelCatalog.contextWindow",
                      "上下文",
                    )}`}
                  />
                ) : null}
              </div>
              {showVision ? (
                <label className="api-model-vision-toggle">
                  <input
                    type="checkbox"
                    checked={visionStates?.[model] ?? false}
                    onChange={(event) =>
                      onVisionChange?.(model, event.target.checked)
                    }
                    disabled={disabled}
                    aria-label={`${model} ${visionLabel}`}
                  />
                  <span className="api-model-vision-switch" />
                </label>
              ) : null}
            </div>
          );
        })}
      </div>
    </div>
  );
}
