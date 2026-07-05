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

local function pyfixy_already_attached(bufnr, root)
  for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr, name = config.name })) do
    if root_for(client) == root then
      return true
    end
  end
  return false
end

local function start(bufnr, ty)
  if vim.bo[bufnr].filetype ~= "python" then
    return
  end

  local root = root_for(ty)
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
    handlers = {
      ["pyfixy/fixtureReturnTypes"] = handle_fixture_return_types,
    },
    init_options = {
      diagnostics = config.diagnostics,
      ty_bridge = true,
    },
  }, { bufnr = bufnr })

  if id then
    started_by_root[root] = id
  end
end

local function extract_hover_type(value, fixture_name)
  if type(value) ~= "string" then
    return nil
  end
  local patterns = {
    "def%s+" .. vim.pesc(fixture_name) .. "%b()%s*%-%>%s*([^:\n`]+)",
    "%)%s*%-%>%s*([^:\n`]+)",
  }
  for _, pattern in ipairs(patterns) do
    local ty = value:match(pattern)
    if ty then
      ty = vim.trim(ty)
      if ty ~= "" and ty ~= "Unknown" then
        return ty
      end
    end
  end
end

local function hover_type_from_result(result, fixture_name)
  if not result or not result.contents then
    return nil
  end
  local contents = result.contents
  if type(contents) == "string" then
    return extract_hover_type(contents, fixture_name)
  end
  if contents.value then
    return extract_hover_type(contents.value, fixture_name)
  end
  if vim.islist(contents) then
    for _, item in ipairs(contents) do
      local text = type(item) == "string" and item or item.value
      local ty = extract_hover_type(text, fixture_name)
      if ty then
        return ty
      end
    end
  end
end

local function handle_fixture_return_types(_err, params, ctx)
  local pyfixy = vim.lsp.get_client_by_id(ctx.client_id)
  local root = pyfixy and root_for(pyfixy)
  local results = {}
  for _, fixture in ipairs(params.fixtures or {}) do
    local path = vim.uri_to_fname(fixture.uri)
    local bufnr = vim.fn.bufadd(path)
    vim.fn.bufload(bufnr)
    local ty_client
    for _, client in ipairs(vim.lsp.get_clients({ bufnr = bufnr })) do
      if is_ty(client) and (not root or root_for(client) == root) then
        ty_client = client
        break
      end
    end
    if not ty_client and root then
      for _, client in ipairs(vim.lsp.get_clients()) do
        if is_ty(client) and root_for(client) == root then
          vim.lsp.buf_attach_client(bufnr, client.id)
          ty_client = client
          break
        end
      end
    end
    if ty_client then
      local response = ty_client:request_sync("textDocument/hover", {
        textDocument = { uri = fixture.uri },
        position = fixture.position,
      }, 800, bufnr)
      local ty = response and not response.err and hover_type_from_result(response.result, fixture.name)
      if ty then
        table.insert(results, { uri = fixture.uri, name = fixture.name, type = ty })
      end
    end
  end
  return results
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
