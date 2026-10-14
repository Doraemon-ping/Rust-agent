import { createRouter, createWebHashHistory } from 'vue-router'

import WorkbenchView from '@/views/WorkbenchView.vue'

/**
 * Hash history on purpose: the packaged app is loaded from a custom scheme, and
 * file-relative routing is the one shape every webview agrees on.
 *
 * The other entries are the workbench's shape, not its features -- phase 1
 * ships a placeholder for each so navigation is honest about what is not built.
 */
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    {
      path: '/',
      name: 'workbench',
      component: WorkbenchView,
      meta: { title: '工作台', icon: 'square-terminal' },
    },
  ],
})
