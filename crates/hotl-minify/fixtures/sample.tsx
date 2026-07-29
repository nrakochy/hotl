// Fixture: TSX mixing plain code with JSX whose whitespace is meaningful.

export interface Props {
  name: string
  count: number
}

// Characters that must survive verbatim: a statement separator and a comment
// marker, both inside a string.
const TRICKY = 'a;b // not a comment'

const RAW = `line one
line two; still inside // the template literal
`

export function label(p: Props): string {
  if (p.count === 0) {
    return TRICKY
  }
  return RAW
}

export function Badge(p: Props) {
  return (
    <span className="badge" title={p.name}>
      {p.count} items
    </span>
  )
}

export function Panel(p: Props) {
  const heading = label(p)
  return (
    <>
      <h2>{heading}</h2>
      <p>
        hello <b>world</b> and <Badge name={p.name} count={p.count} />
      </p>
    </>
  )
}
