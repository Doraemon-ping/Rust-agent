<script setup lang="ts">
import type { Component } from 'vue'

import Badge from '@/components/ui/Badge.vue'
import Card from '@/components/ui/Card.vue'

/**
 * The shape of a page that is not built yet.
 *
 * It carries two honest things: what the page will do, and the facts the
 * runtime already reports about that area. Nothing here is invented -- when
 * there are no facts yet, it says so.
 */
const props = defineProps<{
  title: string
  blurb: string
  icon: Component
  /** Straight from `runtime.status`; empty when the runtime reported none. */
  facts?: { label: string; value: string }[]
  /** What the page will do once it exists. */
  pending: string[]
}>()
</script>

<template>
  <div class="h-full min-h-0 overflow-y-auto p-8 scroll-slim">
    <header class="mb-5">
      <div class="flex items-center gap-2">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">Workbench</p>
        <Badge class="border-line/70 px-1.5 py-0 text-[10px]">待建</Badge>
      </div>
      <h1 class="mt-1 text-xl font-semibold tracking-tight">{{ props.title }}</h1>
      <p class="mt-1 max-w-2xl text-sm text-ink-muted">{{ props.blurb }}</p>
    </header>

    <div class="grid grid-cols-1 gap-5 xl:grid-cols-2">
      <Card>
        <template #header>
          <h2 class="text-base font-semibold tracking-tight">这一页会做什么</h2>
        </template>
        <ul class="space-y-2.5 text-sm text-ink-muted">
          <li v-for="line in props.pending" :key="line" class="flex gap-2.5">
            <component :is="props.icon" class="mt-0.5 size-4 shrink-0 text-ink-faint" />
            <span>{{ line }}</span>
          </li>
        </ul>
      </Card>

      <Card>
        <template #header>
          <h2 class="text-base font-semibold tracking-tight">Runtime 已经报告的事实</h2>
        </template>
        <dl v-if="props.facts?.length" class="grid grid-cols-2 gap-x-6 gap-y-3">
          <div v-for="fact in props.facts" :key="fact.label" class="min-w-0 space-y-0.5">
            <dt class="text-xs text-ink-faint">{{ fact.label }}</dt>
            <dd class="truncate font-mono text-sm text-ink" :title="fact.value">
              {{ fact.value }}
            </dd>
          </div>
        </dl>
        <p v-else class="text-sm text-ink-faint">runtime 还没有上报这类数据。</p>
      </Card>
    </div>
  </div>
</template>
