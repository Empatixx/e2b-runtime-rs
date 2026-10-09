import { HomeLayout } from 'fumadocs-ui/layouts/home'
import type { ReactNode } from 'react'
import { baseOptions } from '@/lib/layout.shared'
import type { Lang } from '@/lib/i18n'

export default async function Layout(props: { params: Promise<{ lang: string }>; children: ReactNode }) {
  const lang = (await props.params).lang as Lang
  return <HomeLayout {...baseOptions(lang)}>{props.children}</HomeLayout>
}
