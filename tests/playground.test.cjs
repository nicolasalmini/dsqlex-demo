const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const html = fs.readFileSync(
  path.join(__dirname, "..", "static", "playground.html"),
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
    style: {},
    selectionStart: 0,
    selectionEnd: 0,
    scrollHeight: 0,
    scrollTop: 0,
    scrollLeft: 0,
    handlers: {},
    children: [],
    attrs: {},
    textContent: "",
    className: "",
    addEventListener(ev, fn) { this.handlers[ev] = fn; },
    setSelectionRange(a, b) { this.selectionStart = a; this.selectionEnd = b; },
    setAttribute(k, v) { this.attrs[k] = v; },
    appendChild(c) { this.children.push(c); },
    querySelectorAll() { return this.children; },
    focus() {},
  };
}

function load({ drafts = {} } = {}) {
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
    sessionStorage: { getItem: k => drafts[k] ?? null, setItem: () => {} },
    AbortController,
    fetch: (url, opts) => {
      const c = { url, body: opts && opts.body, resolve: null };
      c.p = new Promise(r => (c.resolve = r));
      fetchCalls.push(c);
      return c.p;
    },
    setTimeout,
    clearTimeout,
    console,
  };
  vm.createContext(sandbox);
  vm.runInContext(templatesSrc, sandbox);
  vm.runInContext(script, sandbox);
  return { sandbox, els, fetchCalls, TPL: sandbox.DSQLEX_TEMPLATES };
}

const okRes = v => ({
  ok: true,
  status: 200,
  json: async () => ({ status: "ok", value: v, value_type: "decimal" }),
});

const TARGET_EXPR =
  "CASE\n    WHEN country = 'Brazil' THEN amount * 2\n    ELSE amount\nEND";
const OLD_DEFAULT =
  "CASE WHEN country = 'Brazil' THEN amount * 2 ELSE amount END";

test("initial expression is the multiline formatted default", () => {
  const { sandbox, els } = load();
  assert.strictEqual(els.expr.value, TARGET_EXPR);
  assert.strictEqual(vm.runInContext("DEFAULT_EXPR", sandbox), TARGET_EXPR);
});

test("old one-line default draft migrates to multiline", () => {
  const { els } = load({ drafts: { "dsqlex-pg-expr": OLD_DEFAULT } });
  assert.strictEqual(els.expr.value, TARGET_EXPR);
  const { els: els2 } = load({ drafts: { "dsqlex-pg-expr": "amount + 1" } });
  assert.strictEqual(els2.expr.value, "amount + 1");
});

test("template buttons load shared set in order", () => {
  const { els, TPL } = load();
  const btns = els.templates.children;
  assert.strictEqual(btns.length, 4);
  assert.deepStrictEqual(
    btns.map(b => b.dataset.tpl),
    ["field", "arithmetic", "simple-case", "monster-case"]
  );
  assert.strictEqual((TPL[3].expression.match(/WHEN/g) || []).length, 9);
  assert.strictEqual(btns[2].title, TPL[2].description);
});

test("template click fills exact expression and evaluates with raw ctx", async () => {
  const { els, fetchCalls, TPL } = load();
  fetchCalls.length = 0;
  els.expr.value = "x";
  const rawCtx = '{ "rate": 0.1234567890123456789012345678 }';
  els.ctx.value = rawCtx;
  const btn = els.templates.children[2];
  btn.handlers.click();
  assert.strictEqual(els.expr.value, TPL[2].expression);
  assert.strictEqual(els.expr.value, TARGET_EXPR);
  assert.strictEqual(btn.attrs["aria-pressed"], "true");
  await new Promise(r => setTimeout(r, 20));
  const call = fetchCalls.find(c => c.url === "/api/playground/evaluate");
  assert.ok(call);
  const payload = JSON.parse(call.body);
  assert.strictEqual(payload.expression, TARGET_EXPR);
  assert.strictEqual(payload.context_json, rawCtx);
  assert.ok(!fetchCalls.some(c => c.url === "/api/rule" || c.url === "/api/state"));
});

test("monster template grows expression editor", () => {
  const { els } = load();
  els.expr.scrollHeight = 420;
  els.templates.children[3].handlers.click();
  assert.strictEqual(els.expr.style.height, "420px");
  assert.strictEqual(els.expr.value.includes("WHEN country = 'Chile'"), true);
});

test("typing clears selected template state", () => {
  const { els } = load();
  els.templates.children[1].handlers.click();
  els.expr.handlers.input();
  els.templates.children.forEach(b =>
    assert.strictEqual(b.attrs["aria-pressed"], "false")
  );
});

test("context edits preserve selected template; expression edits clear it", () => {
  const { els } = load();
  els.templates.children[3].handlers.click();
  const monster = els.templates.children[3];
  assert.strictEqual(monster.attrs["aria-pressed"], "true");
  els.ctx.value = '{"amount": 5}';
  els.ctx.handlers.input();
  assert.strictEqual(monster.attrs["aria-pressed"], "true");
  els.ctx.value = '{ "amount": 5 }';
  els.ctx.selectionStart = els.ctx.selectionEnd = 1;
  els.ctx.handlers.keydown({ key: "Tab", currentTarget: els.ctx, preventDefault() {} });
  assert.strictEqual(monster.attrs["aria-pressed"], "true");
  els.expr.handlers.input();
  els.templates.children.forEach(b =>
    assert.strictEqual(b.attrs["aria-pressed"], "false")
  );
});

test("Tab indents four spaces in both editors; Shift+Tab outdents with caret", () => {
  const { els } = load();
  const ev = (ta, shift) => ({
    key: "Tab", shiftKey: !!shift, currentTarget: ta, preventDefault() {},
  });
  for (const ta of [els.expr, els.ctx]) {
    ta.value = "ab";
    ta.selectionStart = ta.selectionEnd = 1;
    ta.handlers.keydown(ev(ta));
    assert.strictEqual(ta.value, "a    b");
    assert.strictEqual(ta.selectionStart, 5);
    ta.value = "    WHEN x";
    ta.selectionStart = ta.selectionEnd = 7;
    ta.handlers.keydown(ev(ta, true));
    assert.strictEqual(ta.value, "WHEN x");
    assert.strictEqual(ta.selectionStart, 3);
    assert.strictEqual(ta.selectionEnd, 3);
  }
});

test("autoGrow expands and shrinks both editors", () => {
  const { sandbox, els } = load();
  for (const id of ["expr", "ctx"]) {
    const ta = els[id];
    ta.scrollHeight = 200;
    sandbox.autoGrow(ta);
    assert.strictEqual(ta.style.height, "200px");
    ta.scrollHeight = 90;
    sandbox.autoGrow(ta);
    assert.strictEqual(ta.style.height, "90px");
  }
});

test("debounced eval sends raw context string verbatim", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  const raw = '{ "rate": 0.1234567890123456789012345678 }';
  els.expr.value = "rate";
  els.ctx.value = raw;
  sandbox.onEdit();
  await new Promise(r => setTimeout(r, 250));
  const call = fetchCalls.find(c => c.url === "/api/playground/evaluate");
  assert.ok(call);
  const payload = JSON.parse(call.body);
  assert.strictEqual(payload.context_json, raw);
  assert.strictEqual(payload.expression, "rate");
});

test("stale response cannot overwrite newer result", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  els.expr.value = "1";
  els.ctx.value = "{}";
  sandbox.runEval();
  els.expr.value = "2";
  sandbox.runEval();
  await new Promise(r => setTimeout(r, 20));
  assert.strictEqual(fetchCalls.length, 2);
  fetchCalls[1].resolve(okRes("newer"));
  await new Promise(r => setTimeout(r, 20));
  fetchCalls[0].resolve(okRes("stale"));
  await new Promise(r => setTimeout(r, 20));
  assert.strictEqual(els.rvalue.textContent, "newer");
});

test("error status rendered distinctly", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  els.expr.value = "nope + 1";
  els.ctx.value = "{}";
  sandbox.runEval();
  fetchCalls[0].resolve({
    ok: true,
    status: 200,
    json: async () => ({ status: "evaluation_error", message: "unknown field" }),
  });
  await new Promise(r => setTimeout(r, 20));
  assert.strictEqual(els.result.className, "result err");
  assert.match(els.rvalue.textContent, /evaluation error.*unknown field/);
});

test("blank expression shows hint and sends no request", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  els.expr.value = "   ";
  els.ctx.value = "{}";
  await sandbox.runEval();
  assert.strictEqual(fetchCalls.length, 0);
  assert.match(els.rvalue.textContent, /Type an expression/);
});

test("invalid context JSON errors locally without request", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  els.expr.value = "x";
  els.ctx.value = "{bad";
  await sandbox.runEval();
  assert.strictEqual(fetchCalls.length, 0);
  assert.match(els.rvalue.textContent, /context error/);
});

test("highlight builds DOM nodes, no innerHTML", () => {
  const { sandbox } = load();
  const target = makeEl();
  sandbox.highlight("CASE WHEN a = 'x' THEN 1 -- note", target);
  assert.ok(target.children.length > 0);
  assert.ok(!("innerHTML" in target) || target.innerHTML === undefined);
  const nodes = target.children.flatMap(c =>
    c.children && c.children.length ? c.children : [c]
  );
  const kw = nodes.find(c => c.className === "tok-kw" && c.textContent === "CASE");
  assert.ok(kw);
  const str = nodes.find(
    c => c.className === "tok-str" && c.textContent === "'x'"
  );
  assert.ok(str);
  const com = nodes.find(c => c.className === "tok-com");
  assert.ok(com);
});

function flat(target) {
  return target.children.flatMap(c =>
    c.children && c.children.length ? c.children : [c]
  );
}

test("numeric tokenizer splits 1+2 but keeps 1e-3 whole", () => {
  const { sandbox } = load();
  let target = makeEl();
  sandbox.highlight("1+2", target);
  let nums = flat(target).filter(c => c.className === "tok-num");
  assert.strictEqual(nums.length, 2);
  assert.deepStrictEqual(nums.map(n => n.textContent), ["1", "2"]);
  assert.ok(flat(target).some(c => c.text === "+"));

  target = makeEl();
  sandbox.highlight("1e-3 + x", target);
  nums = flat(target).filter(c => c.className === "tok-num");
  assert.strictEqual(nums.length, 1);
  assert.strictEqual(nums[0].textContent, "1e-3");
});

test("keyword set matches supported DSQLEX names", () => {
  const { sandbox } = load();
  const target = makeEl();
  sandbox.highlight("UPPER(name) WHERE x", target);
  const nodes = flat(target);
  const upper = nodes.find(c => (c.textContent || c.text) === "UPPER");
  assert.strictEqual(upper.className, "tok-kw");
  const where = nodes.find(c => (c.textContent || c.text || "").includes("WHERE"));
  assert.ok(where);
  assert.notStrictEqual(where.className, "tok-kw");
});

test("neutral idle/checking states do not show error styling", () => {
  const { sandbox, els } = load();
  sandbox.showResult(null, "Checking…", null);
  assert.strictEqual(els.result.className, "result");
  sandbox.showResult(null, "Type an expression to evaluate", null);
  assert.strictEqual(els.result.className, "result");
  sandbox.showResult("context_error", "bad");
  assert.strictEqual(els.result.className, "result err");
});

test("edit to invalid JSON invalidates in-flight stale success", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  els.expr.value = "amount";
  els.ctx.value = '{"amount": 1}';
  sandbox.runEval();
  assert.strictEqual(fetchCalls.length, 1);
  els.ctx.value = "{bad";
  sandbox.onEdit();
  assert.match(els.rvalue.textContent, /context error/);
  fetchCalls[0].resolve(okRes("999"));
  await new Promise(r => setTimeout(r, 20));
  assert.match(els.rvalue.textContent, /context error/);
});

test("edit to blank expr invalidates in-flight stale success", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  els.expr.value = "amount";
  els.ctx.value = "{}";
  sandbox.runEval();
  els.expr.value = "";
  sandbox.onEdit();
  assert.match(els.rvalue.textContent, /Type an expression/);
  fetchCalls[0].resolve(okRes("999"));
  await new Promise(r => setTimeout(r, 20));
  assert.match(els.rvalue.textContent, /Type an expression/);
});

test("edit during pending debounce suppresses older response", async () => {
  const { sandbox, els, fetchCalls } = load();
  fetchCalls.length = 0;
  els.expr.value = "a";
  els.ctx.value = "{}";
  sandbox.runEval();
  els.expr.value = "b";
  sandbox.onEdit();
  assert.strictEqual(els.rvalue.textContent, "Checking…");
  fetchCalls[0].resolve(okRes("stale"));
  await new Promise(r => setTimeout(r, 250));
  const b = fetchCalls.find(c => JSON.parse(c.body).expression === "b");
  assert.ok(b);
  b.resolve(okRes("fresh"));
  await new Promise(r => setTimeout(r, 20));
  assert.strictEqual(els.rvalue.textContent, "fresh");
});
