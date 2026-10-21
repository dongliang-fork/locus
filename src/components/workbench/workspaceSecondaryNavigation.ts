import type { DevelopmentResourceRef } from "../../types/workbench";

export type WorkspaceSecondarySection = "knowledge" | "assets" | "views" | "archived" | "sessions" | "archivedFiles" | "fileHistory";
export type WorkspaceOverlaySection = "collab" | "agents";

export function workspaceSecondarySection(kind: string): WorkspaceSecondarySection | null {
  switch (kind) {
    case "sessionsRoot": return "sessions";
    case "knowledgeRoot": return "knowledge";
    case "assetsRoot": return "assets";
    case "viewsRoot": return "views";
    case "archivedRoot": return "archived";
    case "archivedFilesRoot": return "archivedFiles";
    default: return null;
  }
}

/** Workspace nodes whose click covers the secondary list and editor tabs instead of opening either. */
export function workspaceOverlaySection(kind: string): WorkspaceOverlaySection | null {
  switch (kind) {
    case "collaboration": return "collab";
    case "agentsRoot": return "agents";
    default: return null;
  }
}

/** Collaboration and Agent pages left the secondary column; drop lists saved by earlier layouts. */
export function isWorkspaceSecondarySection(section: string): section is WorkspaceSecondarySection {
  return ["knowledge", "assets", "views", "archived", "sessions", "archivedFiles", "fileHistory"].includes(section);
}

export function workspaceSecondaryResourceSection(resource: DevelopmentResourceRef): WorkspaceSecondarySection | null {
  if (resource.kind === "knowledgeRoot") return "knowledge";
  if (resource.kind !== "section") return null;
  if (resource.section === "archived" && resource.sessionId) return null;
  if (resource.section === "knowledge" && resource.knowledgePage) return null;
  return ["knowledge", "assets", "views", "archived"].includes(resource.section)
    ? resource.section as WorkspaceSecondarySection
    : null;
}
