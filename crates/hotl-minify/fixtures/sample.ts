// Fixture: real-shaped, deliberately semicolon-less TypeScript.

// Newline-separated interface members: their `;` is optional, so the tree is
// the only thing that knows a member ended.
export interface Shaped {
  name: string
  count: number
  area(scale: number): number
}

export type Kind = 'point' | 'line'

// Characters that must survive verbatim: a statement separator and a comment
// marker, both inside a string.
const TRICKY = 'a;b // not a comment'

// A template literal spanning lines. One leaf token.
const RAW = `line one
line two; still inside // the template literal
`

export class Record implements Shaped {
  name: string
  count = 0

  constructor(name: string) {
    this.name = name
  }

  area(scale: number): number {
    return this.count * scale
  }

  describe(kind: Kind): string {
    if (kind === 'point') {
      return `${this.name}: point`
    }
    return `${this.name}: line`
  }
}

export function tally(records: Record[]): Map<string, number> {
  const out = new Map<string, number>()
  for (const r of records) {
    // Accumulate per name.
    out.set(r.name, (out.get(r.name) ?? 0) + r.count)
  }
  return out
}

export function firstOrNothing(records: Record[]): Record | undefined {
  if (records.length === 0) {
    return
  }
  return records[0]
}

export function banner(empty: boolean): string {
  return empty || TRICKY.length === 0 ? RAW : TRICKY
}
