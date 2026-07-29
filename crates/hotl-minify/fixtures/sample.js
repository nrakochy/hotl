// Fixture: real-shaped, deliberately semicolon-less JavaScript.

import { readFile } from 'node:fs/promises'

// Characters that must survive verbatim: a statement separator and a comment
// marker, both inside a string.
const TRICKY = 'a;b // not a comment'

// A template literal spanning lines. One leaf token.
const RAW = `line one
line two; still inside // the template literal
`

export class Record {
  constructor(name) {
    this.name = name
    this.count = 0
  }

  describe(kind) {
    switch (kind) {
      case 0:
        return `${this.name}: point`
      case 1:
        return `${this.name}: line`
      default:
        return this.name.toUpperCase()
    }
  }
}

export function tally(records) {
  const out = new Map()
  for (const r of records) {
    // Accumulate per name.
    out.set(r.name, (out.get(r.name) ?? 0) + r.count)
  }
  return out
}

export function firstOrNothing(records) {
  if (records.length === 0) {
    return
  }
  return records[0]
}

export function chained(records) {
  return records
    .filter((r) => r.count > 0)
    .map((r) => r.name)
    .join(', ')
}

export async function load(path) {
  const text = await readFile(path, 'utf8')
  return text.length > 0 ? text : RAW + TRICKY
}
