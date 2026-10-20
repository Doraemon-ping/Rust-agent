<script setup lang="ts">
import { BookOpen, FileText, FolderInput, LoaderCircle, RefreshCw, Sparkles } from '@lucide/vue'
import { computed, onMounted } from 'vue'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import Card from '@/components/ui/Card.vue'
import { shortenPath } from '@/lib/format'
import { useRuntimeStore } from '@/stores/runtime'
import { useSkillsStore } from '@/stores/skills'

const skills = useSkillsStore()
const runtime = useRuntimeStore()

onMounted(() => {
  void skills.refresh()
})

/** The one honest reason the page cannot show the catalogue. */
const issue = computed(() => {
  if (!runtime.shell) return '当前页面不在桌面宿主中：没有 Runtime 通道，读不到技能。'
  if (runtime.state === 'error') {
    return runtime.lastError
      ? `${runtime.lastError.code}: ${runtime.lastError.message}`
      : 'Runtime 处于错误状态，详情见设置。'
  }
  if (!runtime.ready) return 'Runtime 还没有运行。'
  if (!skills.supported) {
    return '这个 Runtime 没有上报 skills.list 方法，因此看不到它加载的技能。'
  }
  return null
})
</script>

<template>
  <div class="h-full min-h-0 overflow-y-auto p-8 scroll-slim">
    <header class="mb-5 flex items-start justify-between gap-4">
      <div class="min-w-0">
        <p class="text-xs tracking-[0.18em] text-ink-faint uppercase">Workbench</p>
        <h1 class="mt-1 text-xl font-semibold tracking-tight">技能</h1>
        <p class="mt-1 max-w-2xl text-sm text-ink-muted">
          技能就是工作目录里的 <span class="font-mono">skills/&lt;名字&gt;/SKILL.md</span>。Runtime
          扫描它们、把名字和一句描述放进系统提示，模型需要时用
          <span class="font-mono">load_skill</span>
          读全文。这一页显示的就是它扫到的东西，不是另存的一份。
        </p>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <Badge v-if="skills.loaded">{{ skills.skills.length }} 个技能</Badge>
        <Button
          v-if="skills.importSupported"
          variant="outline"
          size="sm"
          :disabled="!skills.canImport"
          title="选一个文件夹：它自己含 SKILL.md，或它的每个子文件夹各含一个"
          @click="skills.importFrom()"
        >
          <FolderInput class="size-3.5" />
          {{ skills.importing ? '导入中…' : '导入技能…' }}
        </Button>
        <Button
          variant="outline"
          size="sm"
          :disabled="!skills.ready || skills.loading"
          @click="skills.refresh()"
        >
          <RefreshCw class="size-3.5" :class="skills.loading ? 'animate-spin' : ''" />
          刷新
        </Button>
      </div>
    </header>

    <Alert v-if="issue" tone="info" class="mb-5" title="现在还看不到技能表">
      <p>{{ issue }}</p>
      <p v-if="!runtime.ready" class="text-xs">
        技能由 Runtime 从工作目录读取；它不在时这一页读不到任何东西。
      </p>
    </Alert>

    <Alert v-else-if="skills.error" tone="danger" class="mb-5" :title="skills.error.code">
      <p>{{ skills.error.message }}</p>
    </Alert>

    <Alert v-else-if="skills.imported" tone="ok" class="mb-5" title="导入完成">
      <p>
        已把这个文件夹里的 {{ skills.imported }} 个技能复制进技能目录。下面列的就是
        Runtime 现在扫到的东西，模型从下一轮开始就能用 <span class="font-mono">load_skill</span>
        读它。
      </p>
    </Alert>

    <div class="grid grid-cols-1 gap-5 xl:grid-cols-2">
      <Card>
        <template #header>
          <div class="min-w-0">
            <h2 class="text-base font-semibold tracking-tight">技能目录</h2>
            <p
              v-if="skills.dir"
              class="truncate font-mono text-xs text-ink-faint"
              :title="skills.dir"
            >
              {{ shortenPath(skills.dir, 60) }}
            </p>
          </div>
        </template>

        <ul v-if="skills.skills.length" class="divide-y divide-line/60">
          <li v-for="skill in skills.skills" :key="skill.name">
            <button
              type="button"
              class="flex w-full items-start gap-3 py-3 text-left transition-colors hover:text-ink"
              :class="skill.name === skills.selected ? 'text-ink' : 'text-ink-muted'"
              :disabled="!skills.canRead"
              @click="skills.open(skill.name)"
            >
              <Sparkles
                class="mt-0.5 size-4 shrink-0"
                :class="skill.name === skills.selected ? 'text-accent' : 'text-ink-faint'"
              />
              <span class="min-w-0 flex-1 space-y-1">
                <span class="flex items-center gap-2">
                  <span class="font-mono text-sm text-ink">{{ skill.name }}</span>
                  <Badge
                    v-if="skill.name === skills.selected"
                    variant="accent"
                    class="px-1.5 py-0 text-[10px]"
                  >
                    已打开
                  </Badge>
                </span>
                <span class="block text-sm">{{ skill.description }}</span>
                <span
                  class="block truncate font-mono text-[11px] text-ink-faint"
                  :title="skill.path"
                >
                  {{ shortenPath(skill.path, 56) }}
                </span>
              </span>
            </button>
          </li>
        </ul>

        <div v-else class="py-2 text-sm text-ink-faint">
          <p v-if="skills.loaded">这个工作区还没有技能。</p>
          <p v-else>还没有读到技能表。</p>
        </div>

        <template #footer>
          <span v-if="skills.loading" class="flex items-center gap-2">
            <LoaderCircle class="size-3.5 animate-spin" />
            读取技能目录…
          </span>
          <span v-else>点一条技能，右边显示它的全文；再点一次收起。</span>
        </template>
      </Card>

      <Card>
        <template #header>
          <div class="min-w-0">
            <h2 class="text-base font-semibold tracking-tight">技能全文</h2>
            <p class="truncate font-mono text-xs text-ink-faint" :title="skills.chosen?.path">
              {{ skills.detail?.path ?? skills.chosen?.path ?? '选一条技能看它交给模型的原文' }}
            </p>
          </div>
          <Button v-if="skills.detail" variant="ghost" size="sm" @click="skills.close()">
            收起
          </Button>
        </template>

        <div v-if="skills.reading" class="flex items-center gap-2 py-2 text-sm text-ink-faint">
          <LoaderCircle class="size-4 animate-spin" />
          读取 SKILL.md…
        </div>
        <div v-else-if="skills.detail" class="min-h-0 space-y-3">
          <div class="space-y-1">
            <p class="font-mono text-sm text-ink">{{ skills.detail.name }}</p>
            <p class="text-sm text-ink-muted">{{ skills.detail.description }}</p>
          </div>
          <pre
            class="max-h-[28rem] overflow-auto rounded-lg border border-line bg-surface-raised p-3 font-mono text-xs whitespace-pre-wrap text-ink-muted scroll-slim"
          >{{ skills.detail.content }}</pre>
        </div>
        <div v-else class="flex items-center gap-2 py-2 text-sm text-ink-faint">
          <BookOpen class="size-4 shrink-0" />
          <span>还没有打开任何技能。</span>
        </div>

        <template #footer>
          <span class="flex items-start gap-2">
            <FileText class="mt-px size-3.5 shrink-0" />
            <span>
              这里是文件原文：改了 <span class="font-mono">SKILL.md</span> 再刷新即可，Runtime
              不会替你写它。
            </span>
          </span>
        </template>
      </Card>
    </div>
  </div>
</template>
