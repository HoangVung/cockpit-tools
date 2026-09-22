import type { CodexExperimentalModelDefinition } from '../types/codex';

export function validateModelContext(model: Pick<CodexExperimentalModelDefinition,
  'context_window' | 'auto_compact_token_limit'>): string | null {
  const context = model.context_window;
  const compact = model.auto_compact_token_limit;
  const prefix = 'codex.experimentalModelCatalog.models.validation.';
  if (context === undefined && compact === undefined) return null;
  if (context === undefined || compact === undefined) return prefix + 'contextPair';
  if (!Number.isSafeInteger(context) || context <= 0) return prefix + 'contextWindow';
  if (!Number.isSafeInteger(compact) || compact <= 0) return prefix + 'autoCompact';
  if (compact >= context) return prefix + 'autoCompactRange';
  return null;
}
