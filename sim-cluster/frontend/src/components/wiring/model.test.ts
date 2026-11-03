import { describe, expect, it } from 'vitest'
import type { Topology } from '../../types'
import { buildModel } from './model'

describe('buildModel', () => {
  it('merges opposite edges into one undirected link with stable lex anchors', () => {
    const topo: Topology = {
      boards: [
        {
          id: 'mcu-b',
          kind: 'rl78',
          endpoints: [
            { name: 'tx', direction: 'out', payload: 'uart' },
            { name: 'rx', direction: 'in', payload: 'uart' },
          ],
        },
        {
          id: 'mcu-a',
          kind: 'rl78',
          endpoints: [
            { name: 'tx', direction: 'out', payload: 'uart' },
            { name: 'rx', direction: 'in', payload: 'uart' },
          ],
        },
      ],
      edges: [
        {
          from_board: 'mcu-b',
          from_endpoint: 'tx',
          to_board: 'mcu-a',
          to_endpoint: 'rx',
          payload: 'uart',
        },
        {
          from_board: 'mcu-a',
          from_endpoint: 'tx',
          to_board: 'mcu-b',
          to_endpoint: 'rx',
          payload: 'uart',
        },
      ],
    }
    const model = buildModel(topo)
    expect(model.edges).toHaveLength(1)
    const edge = model.edges[0]
    expect(edge.a).toBe('mcu-a')
    expect(edge.b).toBe('mcu-b')
    expect(edge.lines).toHaveLength(2)

    const a = model.boards.find((b) => b.id === 'mcu-a')!
    const b = model.boards.find((b) => b.id === 'mcu-b')!
    expect(a.right).toHaveLength(1)
    expect(b.left).toHaveLength(1)
    const tipsA = a.right[0].tips.map((t) => t.name).sort()
    expect(tipsA).toEqual(['rx', 'tx'])
    const tipsB = b.left[0].tips.map((t) => t.name).sort()
    expect(tipsB).toEqual(['rx', 'tx'])
  })
})
