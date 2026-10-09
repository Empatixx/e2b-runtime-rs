import { createMDX } from 'fumadocs-mdx/next'

const withMDX = createMDX()

const config = {
  output: 'export',
  basePath: process.env.NEXT_BASE_PATH ?? '',
  trailingSlash: true,
  images: { unoptimized: true },
}

export default withMDX(config)
