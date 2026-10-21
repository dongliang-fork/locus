import { describe, expect, it } from "vitest";
import {
  rankMentionSearchResults,
  type MentionSearchRankable,
} from "../components/chat/mentionSearchRanking";

function result(
  entryKind: MentionSearchRankable["entryKind"],
  name: string,
  relPath: string,
): MentionSearchRankable {
  return {
    entryKind,
    name,
    relPath,
    parentPath: relPath.slice(0, Math.max(0, relPath.lastIndexOf("/"))),
    matchScore: 1,
  };
}

describe("mention search ranking", () => {
  it("places knowledge documents before similarly matching assets", () => {
    const asset = result("asset", "PlayerInput.cs", "Assets/Input/PlayerInput.cs");
    const knowledge = result(
      "knowledge",
      "Input architecture",
      "design/input-architecture.md",
    );

    expect(rankMentionSearchResults([asset, knowledge], "input")).toEqual([
      knowledge,
      asset,
    ]);
  });

  it("applies the knowledge priority to Chinese queries", () => {
    const asset = result("asset", "输入配置", "Assets/Config/输入配置.asset");
    const knowledge = result("knowledge", "输入设计", "design/输入设计.md");

    expect(rankMentionSearchResults([asset, knowledge], "输入")).toEqual([
      knowledge,
      asset,
    ]);
  });

  it("keeps a clearly more accurate asset match first", () => {
    const asset = result("asset", "Input", "Assets/Input");
    const knowledge = result(
      "knowledge",
      "Input architecture",
      "design/input-architecture.md",
    );

    expect(rankMentionSearchResults([knowledge, asset], "input")).toEqual([
      asset,
      knowledge,
    ]);
  });

  it("orders by match class before tab and tree sources without duplicates", () => {
    const tab = { ...result("asset", "PlayerInput.cs", "Assets/PlayerInput.cs"), source: "tab" as const };
    const tree = { ...result("asset", "InputSettings", "Assets/InputSettings"), source: "tree" as const };
    const remote = result("asset", "Input", "Assets/Input");
    const duplicate = result("asset", "PlayerInput.cs", "assets\\PlayerInput.cs");
    const unrelated = { ...result("asset", "Camera", "Assets/Camera"), source: "tab" as const };
    expect(rankMentionSearchResults([remote, duplicate, tree, tab, unrelated], "input")).toEqual([remote, tree, tab]);
  });

  it("ranks matching tabs, then tree entries, then backend results within the same match class", () => {
    const tab = { ...result("asset", "InputTabSettings.cs", "Assets/InputTabSettings.cs"), source: "tab" as const };
    const tree = { ...result("asset", "InputTree", "Assets/InputTree"), source: "tree" as const };
    const remote = result("asset", "InputRemote", "Assets/InputRemote");
    expect(rankMentionSearchResults([remote, tree, tab], "input")).toEqual([tab, tree, remote]);
  });

  it("ranks a file name prefix above names that only contain the query", () => {
    const elora = {
      ...result("asset", "Elora_KalanActions_CombatProfile.asset", "Assets/Characters/Elora/Elora_KalanActions_CombatProfile.asset"),
      source: "tree" as const,
    };
    const glb = result(
      "asset",
      "A_Kalan_Kalan_Attack1.glb",
      "Assets/Characters/C003_Kalan/Animations/Imported/Kalan/AttackCombo_20260911/A_Kalan_Kalan_Attack1.glb",
    );
    expect(rankMentionSearchResults([elora, glb], "A_Kalan")).toEqual([glb, elora]);
  });

  it("ranks any file name match above a path-only match, including from tabs", () => {
    const folderMatch = { ...result("asset", "Foo.prefab", "Assets/Characters/C003_Kalan/Attack/Foo.prefab"), source: "tab" as const };
    const nameMatch = result("asset", "Kalanattack.prefab", "Assets/Characters/C003_Kalan/Prefabs/Kalanattack.prefab");
    expect(rankMentionSearchResults([folderMatch, nameMatch], "attack")).toEqual([nameMatch, folderMatch]);
  });

  it("drops subsequence matches scattered across a long path but keeps close fuzzy names", () => {
    const scattered = result("asset", "Lan.prefab", `Assets/K${"x".repeat(40)}/A${"y".repeat(40)}/Lan.prefab`);
    const close = result("asset", "Kalan.prefab", "Assets/Kalan.prefab");
    expect(rankMentionSearchResults([scattered], "kalan")).toEqual([]);
    expect(rankMentionSearchResults([scattered, close], "kln")).toEqual([close]);
  });

  it.each(["design", "plan", "memory", "skill", "reference"])(
    "merges %s knowledge and workspace hits regardless of provider order",
    (type) => {
      const knowledge = result("knowledge", "《尘之回声》战斗策划案", `${type}/《尘之回声》战斗策划案.md`);
      const file = result("asset", "《尘之回声》战斗策划案.md", `Locus\\knowledge\\${type.toUpperCase()}\\《尘之回声》战斗策划案.md`);
      expect(rankMentionSearchResults([file, knowledge], "战斗策划")).toEqual([knowledge]);
      expect(rankMentionSearchResults([knowledge, file], "战斗策划")).toEqual([knowledge]);
    },
  );

  it.each(["tab", "tree"] as const)("keeps the %s candidate when the same knowledge document is returned by search", (source) => {
    const cached = { ...result("knowledge", "Combat design", "design/combat.md"), source };
    const knowledge = result("knowledge", "Combat design", "design/combat.md");
    const file = result("asset", "combat.md", "Locus/knowledge/Design/combat.md");
    expect(rankMentionSearchResults([file, knowledge, cached], "combat")).toEqual([cached]);
  });

  it("preserves same-named documents at different physical paths", () => {
    const knowledge = result("knowledge", "Combat", "design/combat.md");
    const workspaceFile = result("asset", "Combat", "design/combat.md");
    const otherKnowledge = result("knowledge", "Combat", "design/archive/combat.md");
    const externalFile = result("asset", "Combat", "C:/other/Locus/knowledge/design/combat.md");
    expect(rankMentionSearchResults([knowledge, workspaceFile, otherKnowledge, externalFile], ""))
      .toEqual([knowledge, workspaceFile, otherKnowledge, externalFile]);
  });

  it("retains tab order for empty queries and limits the rendered results", () => {
    const items = Array.from({ length: 500 }, (_, index) => result("asset", `Item${index}`, `Assets/Item${index}`));
    expect(rankMentionSearchResults(items, "")).toEqual(items.slice(0, 120));
    expect(rankMentionSearchResults(items, "Item")).toHaveLength(120);
  });

  it("keeps branches distinct from same-named paths and case variants", () => {
    const folder = result("asset", "dev", "dev");
    const branch = { ...result("branch", "dev", "dev"), parentPath: "" };
    const upper = { ...result("branch", "Dev", "Dev"), parentPath: "" };
    expect(rankMentionSearchResults([folder, branch, upper], "dev")).toHaveLength(3);
  });

  it("matches branches by ref name but not by commit subject metadata", () => {
    const branch = { ...result("branch", "origin/main", "origin/main"), parentPath: "", meta: "Remote branch · 31d4 · input refactor" };
    expect(rankMentionSearchResults([branch], "originmain")).toEqual([branch]);
    expect(rankMentionSearchResults([branch], "refactor")).toEqual([]);
  });

  it("refreshes prepared text when a candidate is renamed", () => {
    const item = result("asset", "Hero", "Assets/Hero");
    expect(rankMentionSearchResults([item], "Hero")).toEqual([item]);
    item.name = "Camera";
    item.relPath = "Assets/Camera";
    expect(rankMentionSearchResults([item], "Hero")).toEqual([]);
    expect(rankMentionSearchResults([item], "Camera")).toEqual([item]);
  });
});
