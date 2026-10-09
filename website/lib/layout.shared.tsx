import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared'
import { REPO_URL } from './source'
import type { Lang } from './i18n'

const labels: Record<Lang, { docs: string; blog: string }> = {
  en: { docs: 'Documentation', blog: 'Blog' },
  cs: { docs: 'Dokumentace', blog: 'Blog' },
}

export function baseOptions(lang: Lang): BaseLayoutProps {
  return {
    nav: {
      title: <span className="font-semibold">e2b-runtime-rs</span>,
      url: `/${lang}`,
    },
    githubUrl: REPO_URL,
    links: [
      { text: labels[lang].docs, url: `/${lang}/docs`, active: 'nested-url' },
      { text: labels[lang].blog, url: `/${lang}/blog`, active: 'nested-url' },
    ],
  }
}
