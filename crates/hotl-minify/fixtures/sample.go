// Package fixture is real-shaped Go for the minifier's property tests.
package fixture

import (
	"fmt"
	"strings"
)

// Record is a parsed row.
type Record struct {
	Name  string
	Count int
}

// Shaped is satisfied by anything with an area.
type Shaped interface {
	Area() int
}

// Tricky holds characters that must survive verbatim: a statement separator
// and a comment marker, both inside a string.
const Tricky = "a;b // not a comment"

// Raw is a backquoted string spanning lines. One leaf token.
const Raw = `line one
line two; still inside // the raw string
`

func NewRecord(name string) *Record {
	return &Record{
		Name:  name,
		Count: 0,
	}
}

func (r *Record) Describe(kind int) string {
	switch kind {
	case 0:
		return fmt.Sprintf("%s: point", r.Name)
	case 1:
		return fmt.Sprintf("%s: line", r.Name)
	default:
		return strings.ToUpper(r.Name)
	}
}

func Tally(records []*Record) map[string]int {
	out := make(map[string]int)
	for _, r := range records {
		// Accumulate per name.
		out[r.Name] += r.Count
	}
	if len(out) == 0 {
		return nil
	}
	return out
}

func Banner(empty bool) string {
	if empty || Tricky == "" {
		return Raw
	}
	return Tricky
}
