import { i18n } from '@/lib/i18n'

export default function Root() {
  const target = `${process.env.NEXT_BASE_PATH ?? ''}/${i18n.defaultLanguage}/`
  return (
    <>
      <meta httpEquiv="refresh" content={`0; url=${target}`} />
      <a href={target}>{target}</a>
    </>
  )
}
