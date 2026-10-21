import { describe, expect, it } from "vitest";
import { wrapMarkdownTables } from "../composables/markdownTableHtml";

const TABLE = "<table><thead><tr><th>A</th></tr></thead><tbody><tr><td>B</td></tr></tbody></table>";
const WRAPPED_TABLE = `<div class="md-table-wrap"><div class="md-table-scroll">${TABLE}</div></div>`;

describe("markdownTableHtml", () => {
  it("wraps table markup in a frame around a dedicated scroll container", () => {
    const html = ["<p>before</p>", TABLE, "<p>after</p>"].join("");

    expect(wrapMarkdownTables(html)).toBe(["<p>before</p>", WRAPPED_TABLE, "<p>after</p>"].join(""));
  });

  it("keeps an already wrapped table at a single wrapper level", () => {
    const wrapped = wrapMarkdownTables(WRAPPED_TABLE);

    expect(wrapped).toBe(WRAPPED_TABLE);
    expect(wrapped.match(/md-table-wrap/g)).toHaveLength(1);
    expect(wrapped.match(/md-table-scroll/g)).toHaveLength(1);
  });

  it("upgrades the legacy single scroll wrapper without touching enclosing markup", () => {
    const html = `<div class="note"><div class="md-table-wrap">${TABLE}</div></div>`;

    expect(wrapMarkdownTables(html)).toBe(`<div class="note">${WRAPPED_TABLE}</div>`);
  });

  it("wraps every table in a document independently", () => {
    const html = `${TABLE}<p>between</p>${WRAPPED_TABLE}`;

    expect(wrapMarkdownTables(html)).toBe(`${WRAPPED_TABLE}<p>between</p>${WRAPPED_TABLE}`);
  });
});
