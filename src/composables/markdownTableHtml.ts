const TABLE_WRAPPER_CLASS = "md-table-wrap";
const TABLE_SCROLL_CLASS = "md-table-scroll";
// Matches a table that already carries a wrapper: the current frame + scroller
// pair, or the single scroll wrapper older renders produced.
const WRAPPED_TABLE_RE = /<div class="md-table-wrap">\s*(?:<div class="md-table-scroll">\s*(<table\b[\s\S]*?<\/table>)\s*<\/div>|(<table\b[\s\S]*?<\/table>))\s*<\/div>/gi;
const TABLE_RE = /<table\b[\s\S]*?<\/table>/gi;

// The outer frame draws the border and clips the rounded corners and edge
// shadows; the inner element is the horizontal scroll container.
export function wrapMarkdownTables(html: string): string {
  if (!/<table\b/i.test(html)) return html;

  const normalizedHtml = html.replace(
    WRAPPED_TABLE_RE,
    (_match, nestedTable: string | undefined, singleTable: string | undefined) => nestedTable ?? singleTable ?? "",
  );
  return normalizedHtml.replace(TABLE_RE, (tableHtml) => (
    `<div class="${TABLE_WRAPPER_CLASS}"><div class="${TABLE_SCROLL_CLASS}">${tableHtml}</div></div>`
  ));
}
