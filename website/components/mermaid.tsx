'use client'

import { useEffect, useId, useState } from 'react'
import { useTheme } from 'next-themes'

export function Mermaid({ chart }: { chart: string }) {
  const id = useId().replaceAll(':', '')
  const { resolvedTheme } = useTheme()
  const [svg, setSvg] = useState('')

  useEffect(() => {
    let cancelled = false
    import('mermaid').then(async ({ default: mermaid }) => {
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: 'loose',
        fontFamily: 'inherit',
        theme: resolvedTheme === 'dark' ? 'dark' : 'default',
      })
      const { svg } = await mermaid.render(`mermaid-${id}`, chart.trim())
      if (!cancelled) setSvg(svg)
    })
    return () => {
      cancelled = true
    }
  }, [chart, id, resolvedTheme])

  return <div className="my-6 flex justify-center overflow-x-auto" dangerouslySetInnerHTML={{ __html: svg }} />
}
