// FPM P1: generate `renderer/client/client.gen.ts` from
// `protocol.schema.json` (the Rust MethodRegistry is the single source of truth).
//
// Usage: `npm run gen:client`; consistency is enforced by
// `renderer/__protocol_client.test.tsx` (fails when the schema changes without regenerating).

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const schemaPath = join(here, '..', 'protocol.schema.json');
const outPath = join(here, '..', 'renderer', 'client', 'client.gen.ts');

/** schema → TS types (supported subset: string/integer/number/boolean/array/object/any). */
function tsType(s) {
  if (!s || typeof s !== 'object' || Object.keys(s).length === 0) return 'unknown';
  switch (s.type) {
    case 'string':
      return 'string';
    case 'integer':
    case 'number':
      return 'number';
    case 'boolean':
      return 'boolean';
    case 'array':
      return `${tsType(s.items)}[]`;
    case 'object': {
      const required = new Set(s.required ?? []);
      const props = Object.entries(s.properties ?? {}).map(
        ([key, value]) => `${key}${required.has(key) ? '' : '?'}: ${tsType(value)}`,
      );
      return `{ ${props.join('; ')} }`;
    }
    default:
      return 'unknown';
  }
}

function camel(name) {
  return name
    .split('.')
    .map((part, index) =>
      index === 0 ? part : part[0].toUpperCase() + part.slice(1),
    )
    .join('');
}

export function generate(schemaJson) {
  const schema = JSON.parse(schemaJson);
  const lines = [];
  lines.push('// AUTO-GENERATED from protocol.schema.json — do not edit by hand.');
  lines.push('// Regenerate with: npm run gen:client');
  lines.push('');
  lines.push(`export const PROTOCOL_VERSION = ${schema.protocolVersion};`);
  lines.push('');
  lines.push('export interface ProtocolMethodMeta {');
  lines.push('  params: unknown;');
  lines.push('  result: unknown;');
  lines.push('  streaming: boolean;');
  lines.push('  permissions: string[];');
  lines.push('}');
  lines.push('');
  lines.push('export const PROTOCOL_METHODS: Record<string, ProtocolMethodMeta> = {');
  for (const method of schema.methods) {
    lines.push(
      `  ${JSON.stringify(method.name)}: { params: ${JSON.stringify(method.params)}, result: ${JSON.stringify(method.result)}, streaming: ${method.streaming}, permissions: ${JSON.stringify(method.permissions)} },`,
    );
  }
  lines.push('};');
  lines.push('');
  lines.push('export interface ProtocolClientMethods {');
  for (const method of schema.methods) {
    lines.push(
      `  ${camel(method.name)}(params: ${tsType(method.params)}): Promise<${tsType(method.result)}>;`,
    );
  }
  lines.push('}');
  lines.push('');
  return lines.join('\n');
}

function main() {
  const schemaJson = readFileSync(schemaPath, 'utf8');
  writeFileSync(outPath, generate(schemaJson));
  console.log(`wrote ${outPath}`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main();
}
