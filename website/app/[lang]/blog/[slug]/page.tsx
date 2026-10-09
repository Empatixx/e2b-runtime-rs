import { DocsBody } from 'fumadocs-ui/layouts/docs/page'
import { InlineTOC } from 'fumadocs-ui/components/inline-toc'
import { notFound } from 'next/navigation'
import type { Metadata } from 'next'
import { blogSource } from '@/lib/source'
import { mdxComponents } from '@/components/mdx'

type Params = { params: Promise<{ lang: string; slug: string }> }

export default async function Post(props: Params) {
  const { lang, slug } = await props.params
  const post = blogSource.getPage([slug], lang)
  if (!post) notFound()
  const MDX = post.data.body
  return (
    <main className="mx-auto w-full max-w-3xl px-4 py-16">
      <p className="text-sm text-fd-muted-foreground">
        {post.data.date} · {post.data.author}
      </p>
      <h1 className="mt-2 text-4xl font-bold tracking-tight">{post.data.title}</h1>
      <p className="mt-3 text-lg text-fd-muted-foreground">{post.data.description}</p>
      <InlineTOC items={post.data.toc} className="mt-8" />
      <DocsBody className="mt-8">
        <MDX components={mdxComponents()} />
      </DocsBody>
    </main>
  )
}

export function generateStaticParams() {
  return blogSource.getPages().map((post) => ({ lang: post.locale, slug: post.slugs[0] }))
}

export async function generateMetadata(props: Params): Promise<Metadata> {
  const { lang, slug } = await props.params
  const post = blogSource.getPage([slug], lang)
  if (!post) notFound()
  return { title: post.data.title, description: post.data.description }
}
