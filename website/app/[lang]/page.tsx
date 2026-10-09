import Link from 'next/link'
import { HomeLayout } from 'fumadocs-ui/layouts/home'
import { baseOptions } from '@/lib/layout.shared'
import { REPO_URL } from '@/lib/source'
import type { Lang } from '@/lib/i18n'

const text: Record<Lang, { lead: string; start: string; cards: { title: string; text: string; href: string }[] }> = {
  en: {
    lead: 'The E2B sandbox runtime in Rust: envd inside every microVM and the orchestrator that runs Firecracker sandboxes on a node. Drop-in compatible with the Go original.',
    start: 'Read the docs',
    cards: [
      { title: 'Concepts', text: 'Sandbox, template, build, snapshot, node.', href: 'docs/concepts' },
      { title: 'envd', text: 'The daemon inside every sandbox and its API.', href: 'docs/envd' },
      { title: 'Orchestrator', text: 'Create, pause and resume microVMs; build templates.', href: 'docs/orchestrator' },
    ],
  },
  cs: {
    lead: 'Běhové prostředí E2B sandboxů v Rustu: envd uvnitř každé microVM a orchestrator, který na nodu spouští Firecracker sandboxy. Plně zaměnitelné s původní Go verzí.',
    start: 'Číst dokumentaci',
    cards: [
      { title: 'Základní pojmy', text: 'Sandbox, šablona, build, snapshot, node.', href: 'docs/concepts' },
      { title: 'envd', text: 'Daemon uvnitř každého sandboxu a jeho API.', href: 'docs/envd' },
      { title: 'Orchestrator', text: 'Vytváření, pause a resume microVM; build šablon.', href: 'docs/orchestrator' },
    ],
  },
}

export default async function Home(props: { params: Promise<{ lang: string }> }) {
  const lang = (await props.params).lang as Lang
  const t = text[lang]
  return (
    <HomeLayout {...baseOptions(lang)}>
      <section className="mx-auto flex max-w-3xl flex-col items-center gap-6 px-4 pb-14 pt-20 text-center">
        <h1 className="text-5xl font-bold tracking-tight">e2b-runtime-rs</h1>
        <p className="text-lg text-fd-muted-foreground">{t.lead}</p>
        <div className="flex gap-3">
          <Link href={`/${lang}/docs`} className="rounded-md bg-fd-primary px-5 py-2.5 text-sm font-medium text-fd-primary-foreground">
            {t.start}
          </Link>
          <a href={REPO_URL} className="rounded-md border px-5 py-2.5 text-sm font-medium hover:bg-fd-accent">
            GitHub
          </a>
        </div>
      </section>
      <section className="mx-auto grid max-w-5xl gap-4 px-4 pb-20 sm:grid-cols-3">
        {t.cards.map((card) => (
          <Link key={card.title} href={`/${lang}/${card.href}`} className="rounded-lg border bg-fd-card p-5 hover:bg-fd-accent">
            <h2 className="font-semibold text-fd-primary">{card.title}</h2>
            <p className="mt-2 text-sm text-fd-muted-foreground">{card.text}</p>
          </Link>
        ))}
      </section>
    </HomeLayout>
  )
}
