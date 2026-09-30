import en from './en'
import tr from './tr'

// Yeni dil: bu klasöre <kod>.ts ekle ve buraya kaydet.
export const locales = {
  en: { name: 'English', messages: en },
  tr: { name: 'Türkçe', messages: tr },
}
export type Lang = keyof typeof locales
