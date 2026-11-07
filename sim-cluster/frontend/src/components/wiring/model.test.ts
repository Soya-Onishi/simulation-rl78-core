import { describe, expect, it } from 'vitest'
import type { Topology } from '../../types'
import { buildModel } from './model'

describe('buildModel', () => {
  it('merges opposite edges into one undirected link with stable lex anchors', () => {
    const topo: Topology = {
      boards: [
        {
          id: 'bbbbbbbbbbbbbbbb',
          label: 'mcu-b',
          kind: 'rl78',
          endpoints: [
            { name: '1111111111111111', label: 'tx', direction: 'out', payload: 'uart' },
            { name: '2222222222222222', label: 'rx', direction: 'in', payload: 'uart' },
          ],
        },
        {
          id: 'aaaaaaaaaaaaaaaa',
          label: 'mcu-a',
          kind: 'rl78',
          endpoints: [
            { name: '1111111111111111', label: 'tx', direction: 'out', payload: 'uart' },
            { name: '2222222222222222', label: 'rx', direction: 'in', payload: 'uart' },
          ],
        },
      ],
      edges: [
        {
          from: {
            path: 'bbbbbbbbbbbbbbbb',
            path_label: 'mcu-b',
            port: '1111111111111111',
            port_label: 'tx',
          },
          to: {
            path: 'aaaaaaaaaaaaaaaa',
            path_label: 'mcu-a',
            port: '2222222222222222',
            port_label: 'rx',
          },
          route: [],
          payload: 'uart',
        },
        {
          from: {
            path: 'aaaaaaaaaaaaaaaa',
            path_label: 'mcu-a',
            port: '1111111111111111',
            port_label: 'tx',
          },
          to: {
            path: 'bbbbbbbbbbbbbbbb',
            path_label: 'mcu-b',
            port: '2222222222222222',
            port_label: 'rx',
          },
          route: [],
          payload: 'uart',
        },
      ],
    }
    const model = buildModel(topo)
    expect(model.edges).toHaveLength(1)
    const edge = model.edges[0]
    expect(edge.a).toBe('aaaaaaaaaaaaaaaa')
    expect(edge.b).toBe('bbbbbbbbbbbbbbbb')
    expect(edge.lines).toHaveLength(2)

    const a = model.boards.find((b) => b.id === 'aaaaaaaaaaaaaaaa')!
    const b = model.boards.find((b) => b.id === 'bbbbbbbbbbbbbbbb')!
    expect(a.label).toBe('mcu-a')
    expect(a.right).toHaveLength(1)
    expect(b.left).toHaveLength(1)
    const tipsA = a.right[0].tips.map((t) => t.name).sort()
    expect(tipsA).toEqual(['rx', 'tx'])
    const tipsB = b.left[0].tips.map((t) => t.name).sort()
    expect(tipsB).toEqual(['rx', 'tx'])
  })
})
