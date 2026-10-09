import type { ReactNode } from 'react'
import type { Metadata } from 'next'
import './global.css'

export const metadata: Metadata = {
  title: {
    template: '%s | e2b-runtime-rs',
    default: 'e2b-runtime-rs: E2B envd and orchestrator in Rust',
  },
  description:
    'A Rust rewrite of the E2B sandbox runtime: the envd daemon inside every microVM and the orchestrator that runs Firecracker sandboxes on a node.',
}

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <body className="flex min-h-screen flex-col overflow-x-hidden">{children}</body>
    </html>
  )
}
