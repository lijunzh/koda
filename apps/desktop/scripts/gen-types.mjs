// TypeScript types for Koda's protocol, generated from the schema the Rust
// types produce (docs/src/protocol-schema.json, which a koda-protocol test
// keeps current). Writes src/protocol/types.gen.ts, which isn't committed:
// the page can't drift from the server it ships with.
import { compile } from 'json-schema-to-typescript';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const app = join(dirname(fileURLToPath(import.meta.url)), '..');
const schemaPath = join(app, '../../docs/src/protocol-schema.json');
const outPath = join(app, 'src/protocol/types.gen.ts');
const schema = JSON.parse(readFileSync(schemaPath, 'utf8'));

// A $ref with sibling keys (a description) makes json-schema-to-typescript
// emit a numbered copy of the type it names (Session1, Turn3…): keep the $ref.
const bareRefs = (v) => {
  if (Array.isArray(v)) return v.map(bareRefs);
  if (v && typeof v === 'object') {
    if (typeof v.$ref === 'string') return { $ref: v.$ref };
    return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, bareRefs(x)]));
  }
  return v;
};
const defs = bareRefs(schema.$defs);
// The schema's root is a catalogue of methods, not a type: a root that
// references every definition makes each one a named export.
const root = {
  title: 'KodaProtocol',
  type: 'object',
  additionalProperties: false,
  properties: Object.fromEntries(Object.keys(defs).map((k) => [k, { $ref: `#/$defs/${k}` }])),
  $defs: defs,
};
let ts = await compile(root, 'KodaProtocol', {
  bannerComment:
    '/* Generated from docs/src/protocol-schema.json by scripts/gen-types.mjs. Do not edit. */',
  additionalProperties: false,
  unreachableDefinitions: true,
  strictIndexSignatures: false,
  format: false,
  declareExternallyReferenced: true,
});
// Method name -> its params and result types, from the catalogue.
const name = (ref) => (ref && ref.$ref ? ref.$ref.split('/').pop() : 'unknown');
const table = (obj, fields) =>
  Object.entries(obj)
    .map(
      ([m, v]) =>
        `  ${JSON.stringify(m)}: { ${fields.map((f) => `${f}: ${v[f] ? name(v[f]) : 'unknown'}`).join('; ')} };`,
    )
    .join('\n');
ts += `\nexport interface Methods {\n${table(schema.methods, ['params', 'result'])}\n}\n`;
ts += `export interface Notifications {\n${table(schema.notifications, ['params'])}\n}\n`;
ts += `export interface ServerRequests {\n${table(schema.serverRequests, ['params', 'result'])}\n}\n`;
mkdirSync(dirname(outPath), { recursive: true });
writeFileSync(outPath, ts);
