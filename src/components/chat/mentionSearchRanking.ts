import {
  SEARCH_MATCH_RANK,
  matchPreparedSearchText,
  prepareSearchText,
  type PreparedSearchText,
} from "../../composables/searchMatcher";

export type MentionSearchEntryKind = "asset" | "knowledge" | "sceneObject" | "view" | "session" | "branch";

export interface MentionSearchRankable {
  name: string;
  relPath: string;
  parentPath: string;
  meta?: string;
  matchScore: number;
  entryKind: MentionSearchEntryKind;
  source?: "tab" | "tree";
}

export const MAX_MENTION_SEARCH_RESULTS = 120;

const preparedFields = new WeakMap<MentionSearchRankable, {
  name: string; relPath: string; parentPath: string; meta?: string;
  matchScore: number; entryKind: MentionSearchEntryKind;
  fields: ReturnType<typeof searchFields>;
}>();

function sourcePriority(result: MentionSearchRankable): number {
  return result.source === "tab" ? 0 : result.source === "tree" ? 1 : 2;
}

export function mentionResultKey(result: Pick<MentionSearchRankable, "relPath"> & { entryKind?: MentionSearchEntryKind }): string {
  // Git ref names are case-sensitive and may equal a workspace path.
  if (result.entryKind === "branch") return `branch:${result.relPath}`;
  const path = result.relPath.replace(/\\/g, "/").replace(/\/+$/, "");
  if (result.entryKind === "sceneObject") return `scene:${path}`;
  // Knowledge refs use type-relative paths; workspace search returns the same
  // files under Locus/knowledge. Compare their physical workspace paths.
  const workspacePath = result.entryKind === "knowledge"
    && /^(?:design|plan|memory|skill|reference)\//i.test(path)
    ? `Locus/knowledge/${path}`
    : path;
  return workspacePath.toLowerCase();
}

interface MentionSearchField {
  text: PreparedSearchText;
  weight: number;
  /** The display name (file name, title, object name) rather than a path. */
  isName: boolean;
}

interface MentionMatch {
  /** Name match > path match; substring-level matches > fuzzy subsequences. */
  tier: number;
  rank: number;
  score: number;
}

function compareMatches(left: MentionMatch, right: MentionMatch): number {
  return right.tier - left.tier || right.rank - left.rank || right.score - left.score;
}

function searchFields(result: MentionSearchRankable): MentionSearchField[] {
  // Session discovery is by title; IDs and archive labels are only metadata.
  // Branches match by ref name; commit subjects in their metadata would add noise.
  if (result.entryKind === "session" || result.entryKind === "branch") {
    return [{ text: prepareSearchText(result.name), weight: 180, isName: true }];
  }
  return [
    {
      text: result.name,
      isName: true,
      weight: result.entryKind === "knowledge"
        ? 210 + Math.min(Math.floor(result.matchScore / 12), 60)
        : result.entryKind === "sceneObject"
          ? 190
          : 180 + Math.min(Math.floor(result.matchScore / 12), 90),
    },
    {
      text: result.relPath,
      isName: false,
      weight: result.entryKind === "knowledge"
        ? 145 + Math.min(Math.floor(result.matchScore / 24), 35)
        : result.entryKind === "sceneObject"
          ? 135
          : 90 + Math.min(Math.floor(result.matchScore / 24), 45),
    },
    { text: result.parentPath, isName: false, weight: 30 },
    { text: result.meta || "", isName: false, weight: 50 },
  ].filter((field) => field.text)
    .map((field) => ({ text: prepareSearchText(field.text), weight: field.weight, isName: field.isName }));
}

function bestFieldMatch(query: PreparedSearchText, fields: MentionSearchField[]): MentionMatch | null {
  let best: MentionMatch | null = null;
  for (const field of fields) {
    const match = matchPreparedSearchText(query, field.text);
    if (!match) continue;
    const fuzzy = match.rank === SEARCH_MATCH_RANK.fuzzy;
    // Characters scattered so widely that the gap penalty used the whole
    // budget are not a meaningful match (typically across a long path).
    if (fuzzy && match.score <= 0) continue;
    const candidate = {
      tier: (fuzzy ? 0 : 2) + (field.isName ? 1 : 0),
      rank: match.rank,
      score: match.score + field.weight,
    };
    if (!best || compareMatches(candidate, best) < 0) best = candidate;
  }
  return best;
}

export function rankMentionSearchResults<T extends MentionSearchRankable>(
  results: T[],
  query: string,
  limit = MAX_MENTION_SEARCH_RESULTS,
): T[] {
  const unique = new Map<string, T>();
  for (const result of results) {
    const key = mentionResultKey(result);
    const previous = unique.get(key);
    if (previous && ((result.entryKind === "view" && previous.entryKind === "asset")
      || (result.entryKind === "asset" && previous.entryKind === "view"))) {
      // A loaded source file keeps its tab/tree priority, but the View supplies
      // its searchable display name, icon and attachment target.
      const view = result.entryKind === "view" ? result : previous;
      const source = sourcePriority(result) < sourcePriority(previous) ? result.source : previous.source;
      unique.set(key, view.source === source ? view : { ...view, source });
      continue;
    }
    if (!previous || sourcePriority(result) < sourcePriority(previous)
      || (sourcePriority(result) === sourcePriority(previous)
        && result.entryKind === "knowledge" && previous.entryKind !== "knowledge")) {
      unique.set(key, result);
    }
  }
  const preparedQuery = prepareSearchText(query);
  const matches: Array<{ item: T; match: MentionMatch }> = [];
  for (const item of unique.values()) {
    if (!preparedQuery.compact) {
      matches.push({ item, match: { tier: 0, rank: 0, score: 0 } });
      continue;
    }
    let cached = preparedFields.get(item);
    if (!cached || cached.name !== item.name || cached.relPath !== item.relPath
      || cached.parentPath !== item.parentPath || cached.meta !== item.meta
      || cached.matchScore !== item.matchScore || cached.entryKind !== item.entryKind) {
      cached = {
        name: item.name, relPath: item.relPath, parentPath: item.parentPath, meta: item.meta,
        matchScore: item.matchScore, entryKind: item.entryKind, fields: searchFields(item),
      };
      preparedFields.set(item, cached);
    }
    const match = bestFieldMatch(preparedQuery, cached.fields);
    if (match) matches.push({ item, match });
  }
  // A name match always outranks a path-only match. Open tabs and workspace
  // tree entries only lead among results of the same tier and match class.
  return matches.sort((a, b) => b.match.tier - a.match.tier
    || b.match.rank - a.match.rank
    || sourcePriority(a.item) - sourcePriority(b.item)
    || b.match.score - a.match.score)
    .slice(0, limit).map((match) => match.item);
}
