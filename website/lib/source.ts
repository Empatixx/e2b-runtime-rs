import { docs } from 'collections/server'
import { loader } from 'fumadocs-core/source'
import { i18n } from './i18n'

export const REPO_URL = 'https://github.com/Empatixx/e2b-runtime-rs'

export const source = loader({
  i18n,
  baseUrl: '/docs',
  source: docs.toFumadocsSource(),
})
