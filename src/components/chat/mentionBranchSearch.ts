import { t } from "../../i18n";
import type { GitBranchesResult } from "../../types";
import type { MentionSearchResult } from "./useMentionWorkspaceCandidates";

function branchMeta(label: string, shortHash: string, message: string): string {
  return [label, shortHash, message].map((part) => part.trim()).filter(Boolean).join(" · ");
}

/**
 * Branches are inserted by their git revision name (`dev`, `origin/main`), so
 * `relPath` holds the exact text the composer writes.
 */
export function mapGitBranchMentionResults(result: GitBranchesResult): MentionSearchResult[] {
  const local = result.local.map((branch): MentionSearchResult => ({
    relPath: branch.name,
    name: branch.name,
    parentPath: "",
    meta: branchMeta(
      t(branch.isCurrent ? "chat.mention.currentBranch" : "chat.mention.localBranch"),
      branch.shortHash,
      branch.message,
    ),
    isDir: false,
    matchScore: 0,
    entryKind: "branch",
  }));
  const remote = result.remotes.flatMap(([remoteName, branches]) => branches.map((branch): MentionSearchResult => {
    const name = `${remoteName}/${branch.name}`;
    return {
      relPath: name,
      name,
      parentPath: "",
      meta: branchMeta(t("chat.mention.remoteBranch"), branch.shortHash, branch.message),
      isDir: false,
      matchScore: 0,
      entryKind: "branch",
    };
  }));
  return [...local, ...remote];
}
