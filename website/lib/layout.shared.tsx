import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared'
import { REPO_URL } from './source'
import type { Lang } from './i18n'

const labels: Record<Lang, { docs: string }> = {
  en: { docs: 'Documentation' },
  cs: { docs: 'Dokumentace' },
}

export function baseOptions(lang: Lang): BaseLayoutProps {
  return {
    nav: {
      title: <span className="font-semibold">e2b-runtime-rs</span>,
      url: `/${lang}`,
    },
    githubUrl: REPO_URL,
    links: [{ text: labels[lang].docs, url: `/${lang}/docs`, active: 'nested-url' }],
  }
}
