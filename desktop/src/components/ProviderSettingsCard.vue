<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'

import Alert from '@/components/ui/Alert.vue'
import Badge from '@/components/ui/Badge.vue'
import Button from '@/components/ui/Button.vue'
import Card from '@/components/ui/Card.vue'
import { formatTime, shortenPath } from '@/lib/format'
import type { ProviderInput } from '@/lib/provider'
import { useProviderStore } from '@/stores/provider'
import { useRuntimeStore } from '@/stores/runtime'

/**
 * The model settings the runtime is started with.
 *
 * Two facts are kept apart on purpose: what the host has stored, and what the
 * running runtime reports. Only the second one is the badge, because only the
 * runtime knows what it is actually using.
 */
const provider = useProviderStore()
const runtime = useRuntimeStore()

/** One place for the field styling, so the five inputs cannot drift apart. */
const FIELD =
  'w-full rounded-md border border-line bg-surface-raised px-3 py-2 font-mono text-sm text-ink ' +
  'placeholder:font-sans placeholder:text-ink-faint/70 focus:border-accent/60 ' +
  'focus:outline-none disabled:opacity-50'

const baseUrl = ref('')
const model = ref('')
const fallbackModel = ref('')
const maxTokens = ref('')
const apiKey = ref('')
/** Set by the clear button: the next save sends an empty key instead of none. */
const clearingKey = ref(false)
const formError = ref<string | null>(null)
const savedAt = ref<string | null>(null)

const runtimeModel = computed(() => runtime.snapshot?.model ?? null)
const configError = computed(() => runtime.snapshot?.config_error ?? null)
const storedKey = computed(() => provider.settings?.hasKey ?? false)

/**
 * What the key field is really asking. An empty box means two different things
 * depending on whether a key is already stored, so it says which.
 */
const keyPlaceholder = computed(() => {
  if (clearingKey.value) return '保存后将清除已存密钥'
  if (!storedKey.value) return '粘贴 API Key'
  const hint = provider.settings?.keyHint
  return `已保存${hint ? `（…${hint}）` : ''}，留空表示不修改`
})

/** True when the form differs from what the host reports. */
const dirty = computed(() => {
  const settings = provider.settings
  if (!settings) return false
  return (
    baseUrl.value.trim() !== settings.baseUrl ||
    model.value.trim() !== (settings.model ?? '') ||
    fallbackModel.value.trim() !== (settings.fallbackModel ?? '') ||
    maxTokens.value.trim() !== (settings.maxTokens === null ? '' : String(settings.maxTokens)) ||
    apiKey.value.trim() !== '' ||
    clearingKey.value
  )
})

const canSave = computed(() => provider.shell && provider.ready && dirty.value && !provider.saving)

/** Fill the form from what the host reports. The 还原 button does this too. */
function adopt(): void {
  const settings = provider.settings
  if (!settings) return
  baseUrl.value = settings.baseUrl
  model.value = settings.model ?? ''
  fallbackModel.value = settings.fallbackModel ?? ''
  maxTokens.value = settings.maxTokens === null ? '' : String(settings.maxTokens)
  apiKey.value = ''
  clearingKey.value = false
  formError.value = null
}

/** `null` for empty, `'invalid'` for anything the host would refuse anyway. */
function parsedMaxTokens(): number | null | 'invalid' {
  const raw = maxTokens.value.trim()
  if (raw === '') return null
  const value = Number(raw)
  if (!Number.isInteger(value) || value < 1 || value > 1_000_000) return 'invalid'
  return value
}

async function submit(): Promise<void> {
  formError.value = null
  const tokens = parsedMaxTokens()
  if (tokens === 'invalid') {
    formError.value = '最大 token 数要是 1 到 1000000 之间的整数，留空表示用 Runtime 自己的默认值。'
    return
  }
  const input: ProviderInput = {
    baseUrl: baseUrl.value,
    model: model.value,
    fallbackModel: fallbackModel.value,
    maxTokens: tokens,
    apiKey: clearingKey.value ? '' : apiKey.value.trim() === '' ? null : apiKey.value.trim(),
  }
  if (await provider.save(input)) {
    savedAt.value = new Date().toISOString()
    adopt()
  }
}

function clearKey(): void {
  apiKey.value = ''
  clearingKey.value = true
}

onMounted(async () => {
  await provider.refresh()
  adopt()
})
</script>

<template>
  <Card>
    <template #header>
      <div class="min-w-0 space-y-1">
        <h2 class="text-base font-semibold tracking-tight">模型与凭据</h2>
        <p class="text-xs text-ink-faint">
          宿主在启动 Runtime 子进程时，把这里保存的值作为环境变量交给它。密钥只写不读：
          保存后它不会回到页面，页面只知道有没有存过。
        </p>
      </div>
      <Badge :variant="runtimeModel ? 'ok' : 'warn'">
        {{ runtimeModel ? 'Runtime 已配置' : 'Runtime 未配置' }}
      </Badge>
    </template>

    <div class="space-y-4">
      <Alert v-if="!provider.shell" tone="warn" title="不在桌面宿主中">
        <p>当前页面读不到也存不了模型设置。请用 <span class="font-mono">pnpm app:dev</span> 启动桌面外壳。</p>
      </Alert>

      <!-- A missing shell already has its own note above; this is for a save
           the host actually refused. -->
      <Alert
        v-if="provider.error && provider.error.code !== 'not_in_shell'"
        tone="danger"
        title="宿主拒绝了这次操作"
      >
        <p class="font-mono text-xs">{{ provider.error.code }}</p>
        <p class="break-words">{{ provider.error.message }}</p>
      </Alert>

      <Alert v-if="provider.settings?.warnings?.length" tone="warn" title="宿主报告了设置文件的问题">
        <ul class="list-disc space-y-0.5 pl-4">
          <li v-for="warning in provider.settings?.warnings" :key="warning">{{ warning }}</li>
        </ul>
      </Alert>

      <div class="grid gap-6 lg:grid-cols-[minmax(0,1fr)_19rem]">
        <form class="space-y-4" @submit.prevent="submit">
          <div class="grid gap-4 sm:grid-cols-2">
            <label class="block space-y-1.5 sm:col-span-2" for="provider-base-url">
              <span class="text-sm font-medium text-ink">API 地址</span>
              <input
                id="provider-base-url"
                v-model="baseUrl"
                :class="FIELD"
                type="text"
                spellcheck="false"
                autocomplete="off"
                :disabled="!provider.shell"
                placeholder="https://api.anthropic.com"
              />
              <span class="block text-xs text-ink-faint">
                <span class="font-mono">ANTHROPIC_BASE_URL</span> · 留空则用 Runtime 自己的默认值
              </span>
            </label>

            <label class="block space-y-1.5 sm:col-span-2" for="provider-api-key">
              <span class="text-sm font-medium text-ink">API Key</span>
              <input
                id="provider-api-key"
                v-model="apiKey"
                :class="FIELD"
                type="password"
                spellcheck="false"
                autocomplete="off"
                :disabled="!provider.shell"
                :placeholder="keyPlaceholder"
                @input="clearingKey = false"
              />
              <span class="block text-xs text-ink-faint">
                <span class="font-mono">ANTHROPIC_API_KEY</span> · 只写不读
                <span v-if="clearingKey" class="text-warn"> · 保存后清除</span>
              </span>
            </label>

            <label class="block space-y-1.5" for="provider-model">
              <span class="text-sm font-medium text-ink">模型 ID</span>
              <input
                id="provider-model"
                v-model="model"
                :class="FIELD"
                type="text"
                spellcheck="false"
                autocomplete="off"
                :disabled="!provider.shell"
                placeholder="claude-sonnet-4-5"
              />
              <span class="block text-xs text-ink-faint">
                <span class="font-mono">MODEL_ID</span>
              </span>
            </label>

            <label class="block space-y-1.5" for="provider-fallback-model">
              <span class="text-sm font-medium text-ink">备用模型</span>
              <input
                id="provider-fallback-model"
                v-model="fallbackModel"
                :class="FIELD"
                type="text"
                spellcheck="false"
                autocomplete="off"
                :disabled="!provider.shell"
                placeholder="可留空"
              />
              <span class="block text-xs text-ink-faint">
                <span class="font-mono">FALLBACK_MODEL_ID</span> · 主模型持续 529 时使用
              </span>
            </label>

            <label class="block space-y-1.5" for="provider-max-tokens">
              <span class="text-sm font-medium text-ink">每次回复的最大 token</span>
              <input
                id="provider-max-tokens"
                v-model="maxTokens"
                :class="FIELD"
                type="number"
                min="1"
                step="1"
                autocomplete="off"
                :disabled="!provider.shell"
                placeholder="可留空"
              />
              <span class="block text-xs text-ink-faint">
                <span class="font-mono">MAX_TOKENS</span> · Runtime 默认 8192
              </span>
            </label>
          </div>

          <Alert v-if="formError" tone="danger">{{ formError }}</Alert>

          <div class="flex flex-wrap items-center gap-2">
            <Button type="submit" :disabled="!canSave">
              {{ provider.saving ? '保存中…' : '保存' }}
            </Button>
            <Button variant="outline" :disabled="!dirty || provider.saving" @click="adopt">
              还原
            </Button>
            <Button
              v-if="storedKey && !clearingKey"
              variant="danger"
              size="sm"
              :disabled="provider.saving"
              @click="clearKey"
            >
              清除已保存的密钥
            </Button>
            <span v-if="savedAt" class="text-xs text-ok">已保存 · {{ formatTime(savedAt) }}</span>
          </div>

          <p class="text-xs text-ink-faint">
            保存后，正在运行的 Runtime 会重启以载入新配置；已停止的不会因此启动。
          </p>
        </form>

        <div class="space-y-4 rounded-lg border border-line bg-surface-raised/40 p-4">
          <h3 class="text-sm font-semibold tracking-tight">Runtime 现在看到的</h3>
          <dl class="space-y-3 text-xs">
            <div class="space-y-0.5">
              <dt class="text-ink-faint">模型</dt>
              <dd class="truncate font-mono text-ink" :title="runtimeModel ?? ''">
                {{ runtimeModel ?? '未配置' }}
              </dd>
            </div>
            <div class="space-y-0.5">
              <dt class="text-ink-faint">配置问题</dt>
              <dd class="break-words text-ink-muted">{{ configError ?? '无' }}</dd>
            </div>
            <div class="space-y-0.5">
              <dt class="text-ink-faint">保存于</dt>
              <dd class="font-mono text-ink-muted">
                {{ formatTime(provider.settings?.updatedAt ?? null) }}
              </dd>
            </div>
          </dl>
          <div class="space-y-1.5">
            <p class="text-xs text-ink-faint">宿主交给 Runtime 的环境变量</p>
            <ul class="flex flex-wrap gap-1.5">
              <li
                v-for="name in provider.settings?.env ?? []"
                :key="name"
                class="rounded border border-line px-1.5 py-0.5 font-mono text-[11px] text-ink-muted"
              >
                {{ name }}
              </li>
            </ul>
            <p class="text-xs text-ink-faint">
              只有填了值的字段才会被设置。留空的字段让 Runtime 用它自己的默认值，或它工作目录里
              <span class="font-mono">.env</span> 的值。
            </p>
          </div>
        </div>
      </div>
    </div>

    <template #footer>
      <p class="truncate" :title="provider.settings?.path ?? ''">
        设置文件 {{ shortenPath(provider.settings?.path ?? '--', 72) }}
      </p>
    </template>
  </Card>
</template>
