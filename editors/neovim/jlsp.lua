-- Neovim lspconfig configuration for jlsp
-- Add this to your init.lua or lua/plugins/lsp.lua

local lspconfig = require('lspconfig')
local configs = require('lspconfig.configs')

if not configs.jlsp then
  configs.jlsp = {
    default_config = {
      cmd = { "jlsp" }, -- ensure 'jlsp' executable is in your $PATH
      filetypes = { "java" },
      root_dir = function(fname)
        return lspconfig.util.root_pattern("pom.xml", "build.gradle", "build.gradle.kts", ".git")(fname)
      end,
      settings = {},
    },
  }
end

lspconfig.jlsp.setup({
  on_attach = function(client, bufnr)
    -- Enable inlay hints in Neovim 0.10+
    if vim.lsp.inlay_hint then
      vim.lsp.inlay_hint.enable(true, { bufnr = bufnr })
    end
  end,
})
