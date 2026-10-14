import { createRouter, createWebHashHistory } from 'vue-router'

import ChatView from '@/views/ChatView.vue'
import CronView from '@/views/CronView.vue'
import PluginsView from '@/views/PluginsView.vue'
import SettingsView from '@/views/SettingsView.vue'
import SkillsView from '@/views/SkillsView.vue'

/**
 * Hash history on purpose: the packaged app is loaded from a custom scheme, and
 * file-relative routing is the one shape every webview agrees on.
 *
 * Every route here is reachable from the rail or the project column. Pages that
 * are not built yet are pages, not dead links: they say so on themselves.
 */
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    {
      path: '/',
      name: 'chat',
      component: ChatView,
      meta: { title: '对话' },
    },
    {
      path: '/cron',
      name: 'cron',
      component: CronView,
      meta: { title: '定时任务' },
    },
    {
      path: '/plugins',
      name: 'plugins',
      component: PluginsView,
      meta: { title: '插件' },
    },
    {
      path: '/skills',
      name: 'skills',
      component: SkillsView,
      meta: { title: '技能' },
    },
    {
      path: '/settings',
      name: 'settings',
      component: SettingsView,
      meta: { title: '设置' },
    },
  ],
})
