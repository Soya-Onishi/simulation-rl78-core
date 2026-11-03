import { describe, expect, it } from 'vitest'
import { formatUart } from './common'

describe('formatUart', () => {
  it('renders printable bytes as hex(char)', () => {
    expect(formatUart(0x41)).toBe('41(A)')
  })

  it('renders non-printable bytes as hex only', () => {
    expect(formatUart(0x0a)).toBe('0A')
  })
})
