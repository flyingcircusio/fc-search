/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ["src/*.rs", "templates/*.html"],
  theme: {
    extend: {
      colors: {
        'fc-green': '#52a46c',
        'fc-midnight': '#002855',
        'fc-blue-gray': '#d5dce7',
      }
    }
  },
  plugins: [],
};
