import defaultMdxComponents from 'fumadocs-ui/mdx'
import type { MDXComponents } from 'mdx/types'
import { Mermaid } from './mermaid'

export function mdxComponents(): MDXComponents {
  return { ...defaultMdxComponents, Mermaid }
}
