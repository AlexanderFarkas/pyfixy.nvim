local M = {}

local function default_cmd()
  local source = debug.getinfo(1, "S").source:sub(2)
  local plugin_root = vim.fn.fnamemodify(source, ":p:h:h:h")
  local local_binary = plugin_root .. "/bin/pyfixy-lsp"
  if vim.fn.executable(local_binary) == 1 then
    return { local_binary }
  end
  return { "pyfixy-lsp" }
end

local defaults = {
  cmd = default_cmd(),
  ty_client_names = { "ty" },
  name = "pyfixy-lsp",
  diagnostics = {
    missing_annotation = "hint",
    mismatched_annotation = "error",
  },
  root_markers = { "pyproject.toml", "pytest.ini", "tox.ini", "setup.cfg", "setup.py", ".git" },
  root_dir = nil,
}
local config = vim.deepcopy(defaults)
local started_by_root = {}

local function is_ty(client)
  for _, name in ipairs(config.ty_client_names) do
    if client.name == name then
      return true
    end
  end
  return false
end

local function root_for(client)
  return client.config and client.config.root_dir or client.root_dir
end

local function root_for_buf(bufnr)
  if config.root_dir then
    local root = config.root_dir(bufnr)
    if root and root ~= "" then
      return vim.fs.normalize(root)
    end
  end

  local name = vim.api.nvim_buf_get_name(bufnr)
  if name == "" then
    return nil
  end

  if vim.fs and vim.fs.root then
    return vim.fs.root(name, config.root_markers)
  end

  local dir = vim.fn.fnamemodify(name, ":p:h")
  while dir and dir ~= "" do
    for _, marker in ipairs(config.root_markers) do
      local candidate = dir .. "/" .. marker
      if vim.loop.fs_stat(candidate) then
        return dir
      end
    end
    local parent = vim.fn.fnamemodify(dir, ":h")
    if parent == dir then
      break
    end
    dir = parent
  end
  return nil
end

local function pyfixy_already_attached(bufnr, root)
  for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr, name = config.name })) do
    if root_for(client) == root then
      return true
    end
  end
  return false
end

local function start_with_root(bufnr, root)
  if vim.bo[bufnr].filetype ~= "python" then
    return
  end

  if not root or root == "" or pyfixy_already_attached(bufnr, root) then
    return
  end

  local id = started_by_root[root]
  if id and vim.lsp.get_client_by_id(id) then
    vim.lsp.buf_attach_client(bufnr, id)
    return
  end

  id = vim.lsp.start({
    name = config.name,
    cmd = config.cmd,
    root_dir = root,
    filetypes = { "python" },
    single_file_support = false,
    init_options = {
      diagnostics = config.diagnostics,
    },
  }, { bufnr = bufnr })

  if id then
    started_by_root[root] = id
  end
end

local function start(bufnr, ty)
  start_with_root(bufnr, root_for(ty))
end


local function maybe_start(bufnr)
  if vim.bo[bufnr].filetype ~= "python" then
    return
  end

  for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr })) do
    if is_ty(client) then
      start(bufnr, client)
      return
    end
  end

  start_with_root(bufnr, root_for_buf(bufnr))
end

function M.setup(opts)
  config = vim.tbl_deep_extend("force", vim.deepcopy(defaults), opts or {})

  vim.api.nvim_create_autocmd("LspAttach", {
    group = vim.api.nvim_create_augroup("pyfixy_sidecar", { clear = true }),
    callback = function(args)
      local client = vim.lsp.get_client_by_id(args.data.client_id)
      if client and is_ty(client) then
        start(args.buf, client)
      end
    end,
  })

  vim.api.nvim_create_autocmd("FileType", {
    group = vim.api.nvim_create_augroup("pyfixy_sidecar_filetype", { clear = true }),
    pattern = "python",
    callback = function(args)
      maybe_start(args.buf)
    end,
  })

  vim.schedule(function()
    maybe_start(vim.api.nvim_get_current_buf())
  end)
end

return M
