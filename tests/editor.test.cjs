const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const html = fs.readFileSync(
  path.join(__dirname, "..", "static", "index.html"),
  "utf8"
);
const script = html.match(/<script>([\s\S]*)<\/script>/)[1];
const templatesSrc = fs.readFileSync(
  path.join(__dirname, "..", "static", "templates.js"),
  "utf8"
);

function makeEl() {
  return {
    value: "",
    dataset: {},
    selectionStart: 0,
    selectionEnd: 0,
    handlers: {},
    style: {},
    clientHeight: 0,
    scrollHeight: 0,
    attrs: {},
    children: [],
    addEventListener(ev, fn) { this.handlers[ev] = fn; },
    setSelectionRange(a, b) { this.selectionStart = a; this.selectionEnd = b; },
    setAttribute(k, v) { this.attrs[k] = v; },
    appendChild(c) { this.children.push(c); },
    querySelectorAll() { return this.children; },
    focus() { this.focused = true; },
    textContent: "",
    className: "",
    disabled: false,
  };
}

const els = {};
const fetchCalls = [];
const sandbox = {
  document: {
    getElementById: id => (els[id] = els[id] || makeEl()),
    querySelectorAll: () => [],
    documentElement: { dataset: {} },
    createElement: () => makeEl(),
    createTextNode: t => ({ text: t }),
    createDocumentFragment: () => makeEl(),
    activeElement: null,
  },
  localStorage: { getItem: () => null, setItem: () => {} },
  fetch: (url, opts) => {
    fetchCalls.push({ url, body: opts && opts.body });
    return new Promise(() => {});
  },
  setTimeout: () => 0,
  clearTimeout: () => {},
  console,
};
vm.createContext(sandbox);
vm.runInContext(templatesSrc, sandbox);
vm.runInContext(script, sandbox);

const { formatRuleText, editorKeydown } = sandbox;
const TPL = vm.runInContext("DSQLEX_TEMPLATES", sandbox);

test("shared templates load in order with 9 monster branches", () => {
  assert.deepStrictEqual(
    [...TPL].map(t => t.id),
    ["field", "arithmetic", "simple-case", "monster-case"]
  );
  assert.strictEqual((TPL[3].expression.match(/WHEN/g) || []).length, 9);
  const btns = els.templates.children;
  assert.strictEqual(btns.length, 4);
  assert.deepStrictEqual(
    btns.map(b => b.textContent),
    ["Field value", "Arithmetic", "Simple CASE", "Monster CASE"]
  );
  assert.deepStrictEqual(
    btns.map(b => b.dataset.tpl),
    [...TPL].map(t => t.id)
  );
});

test("template click fills exact expression without calling rule API", () => {
  fetchCalls.length = 0;
  for (let i = 0; i < TPL.length; i++) {
    const btn = els.templates.children[i];
    els.expr.value = "old";
    btn.handlers.click();
    assert.strictEqual(els.expr.value, TPL[i].expression);
    assert.strictEqual(els.expr.dataset.touched, "1");
    assert.strictEqual(btn.attrs["aria-pressed"], "true");
    els.templates.children.forEach((b, j) =>
      assert.strictEqual(b.attrs["aria-pressed"], String(j === i))
    );
    assert.strictEqual(els.msg.textContent, TPL[i].label + " loaded; click Apply rule to use it.");
  }
  assert.ok(!fetchCalls.some(c => c.url === "/api/rule"));
});

test("typing clears selected template state", () => {
  els.templates.children[3].handlers.click();
  els.expr.handlers.input();
  els.templates.children.forEach(b =>
    assert.strictEqual(b.attrs["aria-pressed"], "false")
  );
});

test("monster template grows textarea capped at 300px", () => {
  els.expr.clientHeight = 60;
  els.expr.scrollHeight = 500;
  els.templates.children[3].handlers.click();
  assert.strictEqual(els.expr.style.height, "300px");
  els.expr.clientHeight = 400;
  els.expr.scrollHeight = 500;
  els.templates.children[0].handlers.click();
  assert.strictEqual(els.expr.style.height, "300px");
});

test("formats standard CASE expression exactly", () => {
  const src =
    "CASE WHEN country = 'Brazil' THEN amount * 2 WHEN country = 'Argentina' THEN amount * 3 ELSE amount END";
  const r = formatRuleText(src);
  assert.ok(r.ok);
  assert.strictEqual(
    r.text,
    "CASE\n    WHEN country = 'Brazil' THEN amount * 2\n    WHEN country = 'Argentina' THEN amount * 3\n    ELSE amount\nEND"
  );
});

test("nested CASE indents two spaces deeper", () => {
  const src =
    "CASE WHEN a = 'x' THEN CASE WHEN b = 'y' THEN 1 ELSE 2 END ELSE 0 END";
  const r = formatRuleText(src);
  assert.strictEqual(
    r.text,
    "CASE\n    WHEN a = 'x' THEN CASE\n        WHEN b = 'y' THEN 1\n        ELSE 2\n    END\n    ELSE 0\nEND"
  );
});

test("formatting is idempotent", () => {
  const src =
    "CASE WHEN country = 'Brazil' THEN amount * 2 ELSE amount END";
  const once = formatRuleText(src).text;
  assert.strictEqual(formatRuleText(once).text, once);
});

test("lowercase keywords normalize to uppercase", () => {
  const r = formatRuleText("case when a = 1 then 2 else 3 end");
  assert.strictEqual(r.text, "CASE\n    WHEN a = 1 THEN 2\n    ELSE 3\nEND");
});

test("keywords inside string literals are preserved", () => {
  const src = "CASE WHEN note = 'CASE WHEN END else' THEN 1 ELSE 2 END";
  const r = formatRuleText(src);
  assert.strictEqual(
    r.text,
    "CASE\n    WHEN note = 'CASE WHEN END else' THEN 1\n    ELSE 2\nEND"
  );
});

test("non-CASE expression only trimmed", () => {
  const r = formatRuleText("  amount * 2 +  1  ");
  assert.ok(r.ok);
  assert.strictEqual(r.text, "amount * 2 +  1");
});

test("comments outside literals fail closed", () => {
  for (const src of [
    "CASE WHEN a = 1 THEN 2 END -- note",
    "CASE WHEN a = 1 THEN 2 END # note",
    "CASE WHEN a = 1 THEN 2 /* c */ END",
  ]) {
    const r = formatRuleText(src);
    assert.ok(!r.ok);
    assert.strictEqual(r.text, src);
    assert.match(r.message, /comments or an unfinished string/);
  }
});

test("unmatched quote fails closed", () => {
  const src = "CASE WHEN a = 'x THEN 1 ELSE 2 END";
  const r = formatRuleText(src);
  assert.ok(!r.ok);
  assert.strictEqual(r.text, src);
});

function key(ta, init) {
  let prevented = false;
  editorKeydown(
    Object.assign({ target: ta, preventDefault: () => (prevented = true) }, init)
  );
  return prevented;
}

test("Tab inserts four spaces at caret", () => {
  const ta = makeEl();
  ta.value = "ab";
  ta.selectionStart = ta.selectionEnd = 1;
  assert.ok(key(ta, { key: "Tab" }));
  assert.strictEqual(ta.value, "a    b");
  assert.strictEqual(ta.selectionStart, 5);
  assert.strictEqual(ta.dataset.touched, "1");
});

test("Tab indents each selected line, Shift+Tab outdents", () => {
  const ta = makeEl();
  ta.value = "one\ntwo\nthree";
  ta.selectionStart = 0;
  ta.selectionEnd = 7;
  key(ta, { key: "Tab" });
  assert.strictEqual(ta.value, "    one\n    two\nthree");
  ta.selectionStart = 0;
  ta.selectionEnd = ta.value.length;
  key(ta, { key: "Tab", shiftKey: true });
  assert.strictEqual(ta.value, "one\ntwo\nthree");
});

test("Shift+Tab with collapsed caret keeps caret, does not select line", () => {
  const ta = makeEl();
  ta.value = "    WHEN x THEN 1";
  ta.selectionStart = ta.selectionEnd = 10;
  assert.ok(key(ta, { key: "Tab", shiftKey: true }));
  assert.strictEqual(ta.value, "WHEN x THEN 1");
  assert.strictEqual(ta.selectionStart, 6);
  assert.strictEqual(ta.selectionEnd, 6);
  const s = ta.selectionStart;
  ta.value = ta.value.slice(0, s) + "z" + ta.value.slice(s);
  assert.strictEqual(ta.value, "WHEN xz THEN 1");
});

test("Shift+Tab on line without leading spaces leaves caret unchanged", () => {
  const ta = makeEl();
  ta.value = "WHEN x THEN 1";
  ta.selectionStart = ta.selectionEnd = 5;
  key(ta, { key: "Tab", shiftKey: true });
  assert.strictEqual(ta.value, "WHEN x THEN 1");
  assert.strictEqual(ta.selectionStart, 5);
});

test("Esc then Tab is not trapped", () => {
  const ta = makeEl();
  ta.value = "x";
  ta.selectionStart = ta.selectionEnd = 1;
  assert.ok(!key(ta, { key: "Escape" }));
  assert.ok(!key(ta, { key: "Tab" }));
  assert.strictEqual(ta.value, "x");
  assert.ok(key(ta, { key: "Tab" }));
  assert.strictEqual(ta.value, "x    ");
});

test("render shows TPS grouped with one decimal", () => {
  const render = vm.runInContext("render", sandbox);
  const base = {
    running: false, rows: 0, errors: 0, cycles: 0,
    requested_version: 1, applied_version: 1,
    input_path: "", output_paths: [], source_headers: [],
    source_preview: [], result_headers: [], destination_preview: [],
    last_error: "", expression: "amount",
  };
  const fmt = n => n.toLocaleString(undefined, { minimumFractionDigits: 1, maximumFractionDigits: 1 });
  for (const tps of [1572624.1, 1500, 0]) {
    render({ ...base, tps });
    assert.strictEqual(els.tps.textContent, fmt(tps));
  }
});

test("Ctrl+Tab is not intercepted", () => {
  const ta = makeEl();
  ta.value = "x";
  ta.selectionStart = ta.selectionEnd = 1;
  assert.ok(!key(ta, { key: "Tab", ctrlKey: true }));
  assert.strictEqual(ta.value, "x");
});

test("blur resets armed Esc so re-entry Tab indents", () => {
  const ta = els.expr;
  ta.value = "x";
  ta.selectionStart = ta.selectionEnd = 1;
  assert.ok(!key(ta, { key: "Escape" }));
  ta.handlers.blur();
  assert.ok(key(ta, { key: "Tab" }));
  assert.strictEqual(ta.value, "x    ");
});

test("format grows textarea up to 300px, respects larger heights", () => {
  const ta = els.expr;
  ta.value = "CASE WHEN a = 1 THEN 2 ELSE 3 END";
  ta.scrollHeight = 180;
  ta.clientHeight = 124;
  ta.style = {};
  els.fmt.handlers.click();
  assert.strictEqual(ta.style.height, "180px");
  ta.value = "CASE WHEN a = 1 THEN 2 ELSE 3 END";
  ta.scrollHeight = 999;
  ta.clientHeight = 124;
  ta.style = {};
  els.fmt.handlers.click();
  assert.strictEqual(ta.style.height, "300px");
  ta.clientHeight = 350;
  ta.scrollHeight = 400;
  ta.style = { height: "350px" };
  els.fmt.handlers.click();
  assert.strictEqual(ta.style.height, "350px");
});

test("nested CASE format is idempotent", () => {
  const once = formatRuleText(
    "CASE WHEN a = 'x' THEN CASE WHEN b = 'y' THEN 1 ELSE 2 END ELSE 0 END"
  ).text;
  assert.strictEqual(formatRuleText(once).text, once);
});

test("Start posts empty body; format never calls rule API", () => {
  fetchCalls.length = 0;
  els.expr.value = "CASE WHEN a = 1 THEN 2 ELSE 3 END";
  els.fmt.handlers.click();
  assert.ok(!fetchCalls.some(c => c.url === "/api/rule"));
  els.start.handlers.click();
  const startCall = fetchCalls.find(c => c.url === "/api/start");
  assert.ok(startCall);
  assert.deepStrictEqual(JSON.parse(startCall.body), {});
});
