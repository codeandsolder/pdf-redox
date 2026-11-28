const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const pkg = path.resolve(process.argv[2] ?? 'target/wasm-node');
const { analyze, optimize_with_hidden_text } = require(path.join(pkg, 'pdf_redox_wasm.js'));
const objects = [
  '<< /Type /Catalog /Pages 2 0 R >>',
  '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
  '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 4 0 R >>',
  '<< /Length 4 >>\nstream\nq\nQ\nendstream',
];
let pdf = '%PDF-1.4\n';
const offsets = objects.map((object, i) => {
  const offset = Buffer.byteLength(pdf);
  pdf += `${i + 1} 0 obj\n${object}\nendobj\n`;
  return offset;
});
const xref = Buffer.byteLength(pdf);
pdf += `xref\n0 ${offsets.length + 1}\n0000000000 65535 f \n`;
for (const offset of offsets) pdf += `${String(offset).padStart(10, '0')} 00000 n \n`;
pdf += `trailer\n<< /Size ${offsets.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
const input = new Uint8Array(Buffer.from(pdf));
assert.equal(analyze(input).input_bytes, input.length);
const policy = { remove_categories: [], overrides: {} };
const result = optimize_with_hidden_text(input, 'optimize', 'none', policy);
assert(result.pdf instanceof Uint8Array);
assert.equal(result.report.before.input_bytes, input.length);
assert.equal(Buffer.from(result.pdf.subarray(0, 5)).toString(), '%PDF-');
assert(JSON.stringify(result.report).length > 20);
assert.throws(() => optimize_with_hidden_text(input, 'optimize', 'none', { ...policy, remove_categories: ['not-a-category'] }));
const imports = WebAssembly.Module.imports(new WebAssembly.Module(fs.readFileSync(path.join(pkg, 'pdf_redox_wasm_bg.wasm'))));
assert(!imports.some(({ name }) => /eval|newnoargs|new_with_args/i.test(name)));
console.log(`WASM smoke passed: ${input.length} -> ${result.pdf.length} bytes, no dynamic-eval imports`);
