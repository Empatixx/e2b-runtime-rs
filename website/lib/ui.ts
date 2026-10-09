import { defineI18nUI } from 'fumadocs-ui/i18n'
import { i18n } from './i18n'

export const i18nUI = defineI18nUI(i18n, {
  en: { displayName: 'English' },
  cs: {
    displayName: 'Čeština',
    'Search(search trigger)': 'Hledat',
    'Search(search dialog)': 'Hledat',
    'No results found(search dialog)': 'Nic nenalezeno',
    'On this page(table of contents)': 'Na této stránce',
    'No Headings(table of contents)': 'Bez nadpisů',
    'Next Page(pagination)': 'Další',
    'Previous Page(pagination)': 'Předchozí',
    'Choose a language(language switcher)': 'Jazyk',
    'Edit on GitHub(edit page)': 'Upravit na GitHubu',
    'Last updated on(page footer)': 'Naposledy upraveno',
    'Page Not Found(404 page)': 'Stránka nenalezena',
    'Back to Home(404 page)': 'Zpět na úvod',
  },
})
