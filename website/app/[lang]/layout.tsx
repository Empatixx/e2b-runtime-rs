import { RootProvider } from 'fumadocs-ui/provider/next'
import type { ReactNode } from 'react'
import { i18n } from '@/lib/i18n'
import { i18nUI } from '@/lib/ui'

export default async function LangLayout(props: { params: Promise<{ lang: string }>; children: ReactNode }) {
  const { lang } = await props.params
  return (
    <RootProvider
      i18n={i18nUI.provider(lang)}
      search={{ options: { type: 'static', api: `${process.env.NEXT_BASE_PATH ?? ''}/api/search` } }}
    >
      {props.children}
    </RootProvider>
  )
}

export function generateStaticParams() {
  return i18n.languages.map((lang) => ({ lang }))
}
