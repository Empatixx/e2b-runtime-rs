import { DocsLayout } from 'fumadocs-ui/layouts/docs'
import type { ReactNode } from 'react'
import { baseOptions } from '@/lib/layout.shared'
import { source } from '@/lib/source'
import type { Lang } from '@/lib/i18n'

export default async function Layout(props: { params: Promise<{ lang: string }>; children: ReactNode }) {
  const lang = (await props.params).lang as Lang
  return (
    <DocsLayout tree={source.getPageTree(lang)} {...baseOptions(lang)}>
      {props.children}
    </DocsLayout>
  )
}
