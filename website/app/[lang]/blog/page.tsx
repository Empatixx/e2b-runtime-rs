import Link from 'next/link'
import type { Metadata } from 'next'
import { blogSource } from '@/lib/source'
import type { Lang } from '@/lib/i18n'

const heading: Record<Lang, { title: string; lead: string }> = {
  en: { title: 'Blog', lead: 'How e2b-runtime-rs is built, measured and tested.' },
  cs: { title: 'Blog', lead: 'Jak se e2b-runtime-rs staví, měří a testuje.' },
}

export const metadata: Metadata = { title: 'Blog' }

export default async function BlogIndex(props: { params: Promise<{ lang: string }> }) {
  const lang = (await props.params).lang as Lang
  const posts = blogSource
    .getPages(lang)
    .toSorted((a, b) => b.data.date.localeCompare(a.data.date))
  return (
    <main className="mx-auto w-full max-w-3xl px-4 py-16">
      <h1 className="text-4xl font-bold tracking-tight">{heading[lang].title}</h1>
      <p className="mt-2 text-fd-muted-foreground">{heading[lang].lead}</p>
      <div className="mt-10 flex flex-col gap-4">
        {posts.map((post) => (
          <Link key={post.url} href={post.url} className="rounded-lg border bg-fd-card p-5 hover:bg-fd-accent">
            <p className="text-xs text-fd-muted-foreground">
              {post.data.date} · {post.data.author}
            </p>
            <h2 className="mt-1 text-xl font-semibold">{post.data.title}</h2>
            <p className="mt-1 text-sm text-fd-muted-foreground">{post.data.description}</p>
          </Link>
        ))}
      </div>
    </main>
  )
}
